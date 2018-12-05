//! A library to help grow the stack when it runs out of space.
//!
//! This is an implementation of manually instrumented segmented stacks where
//! points in a program's control flow are annotated with "maybe grow the stack
//! here". Each point of annotation indicates how far away from the end of the
//! stack it's allowed to be, plus the amount of stack to allocate if it does
//! reach the end.
//!
//! Once a program has reached the end of its stack, a temporary stack on the
//! heap is allocated and is switched to for the duration of a closure.
//!
//! # Examples
//!
//! ```
//! // Grow the stack if we are within the "red zone" of 32K, and if we allocate
//! // a new stack allocate 1MB of stack space.
//! //
//! // If we're already in bounds, however, just run the provided closure on our
//! // own stack
//! stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
//!     // guaranteed to have at least 32K of stack
//! });
//! ```

#![allow(improper_ctypes)]

#[macro_use]
extern crate cfg_if;
extern crate libc;
#[cfg(windows)]
extern crate winapi;

use std::cell::Cell;

/// Grows the call stack if necessary.
///
/// This function is intended to be called at manually instrumented points in a
/// program where recursion is known to happen quite a bit. This function will
/// check to see if we're within `red_zone` bytes of the end of the stack, and
/// if so it will allocate a new stack of size `stack_size`.
///
/// The closure `f` is guaranteed to run on a stack with at least `red_zone`
/// bytes, and it will be run on the current stack if there's space available.
#[inline(always)]
pub fn maybe_grow<R, F: FnOnce() -> R>(
    red_zone: usize,
    stack_size: usize,
    f: F,
) -> R {
    if cfg!(unsupported) {
        return f();
    }
    // if we can't guess the remaining stack (unsupported on some platforms)
    // we immediately grow the stack and then cache the new stack size (which
    // we do know now because we know by how much we grew the stack)
    if remaining_stack().map_or(false, |remaining| remaining >= red_zone) {
        f()
    } else {
        grow(stack_size, f)
    }
}

extern {
    fn __stacker_stack_pointer() -> usize;
}

/// Queries the amount of remaining stack as interpreted by this library.
///
/// This function will return the amount of stack space left which will be used
/// to determine whether a stack switch should be made or not.
#[inline(always)]
pub fn remaining_stack() -> Option<usize> {
    get_stack_limit().map(|limit| unsafe { __stacker_stack_pointer() - limit })
}

/// Always creates a new stack for the passed closure to run on.
/// The closure will still be on the same thread as the caller of `grow`.
/// This will allocate a new stack with at least `stack_size` bytes.
#[inline(never)]
pub fn grow<R, F: FnOnce() -> R>(stack_size: usize, f: F) -> R {
    let mut f = Some(f);
    let mut ret = None;
    _grow(stack_size, &mut || {
        ret = Some(f.take().unwrap()());
    });
    ret.unwrap()
}

thread_local! {
    static STACK_LIMIT: Cell<Option<usize>> = Cell::new(unsafe {
        guess_os_stack_limit()
    })
}

#[inline(always)]
fn get_stack_limit() -> Option<usize> {
    STACK_LIMIT.with(|s| s.get())
}

#[cfg(not(unsupported))]
fn set_stack_limit(l: Option<usize>) {
    STACK_LIMIT.with(|s| s.set(l))
}

cfg_if! {
    if #[cfg(unsupported)] {
        fn _grow(stack_size: usize, f: &mut FnMut()) {
            drop(stack_size);
            f();
        }
    } else if #[cfg(all(target_arch = "wasm32", target_os = "unknown"))] {
        extern "C" {
            fn __stacker_switch_stacks(
                new_stack: usize,
                fnptr: *const u8,
                dataptr: *mut u8
            );
        }

        fn _grow(stack_size: usize, mut f: &mut FnMut()) {        
            // Keep stack 4 bytes aligned.
            let stack_size = (stack_size + 3) / 4 * 4;

            // Allocate some new stack for oureslves
            let mut stack = Vec::<u8>::with_capacity(stack_size);
            let new_limit = stack.as_ptr() as usize + 32 * 1024;

            // Save off the old stack limits
            let old_limit = get_stack_limit();

            // Prepare stack limits for the stack switch
            set_stack_limit(Some(new_limit));

            unsafe {
                __stacker_switch_stacks(stack.as_mut_ptr() as usize + stack_size,
                                        doit as usize as *const _,
                                        &mut f as *mut &mut FnMut() as *mut u8);
            }

            // Once we've returned reset bothe stack limits and then return value same
            // value the closure returned.
            set_stack_limit(old_limit);

            unsafe extern fn doit(f: &mut &mut FnMut()) {
                f();
            }
        }
    } else if #[cfg(not(windows))] {
        use std::any::Any;
        use std::panic::{self, AssertUnwindSafe};

        extern {
            fn __stacker_switch_stacks(dataptr: *mut u8,
                                       fnptr: *const u8,
                                       new_stack: usize);
            fn getpagesize() -> libc::c_int;
        }

        struct StackSwitch {
            map: *mut libc::c_void,
            stack_size: usize,
            old_stack_limit: Option<usize>,
        }

        impl Drop for StackSwitch {
            fn drop(&mut self) {
                unsafe {
                    libc::munmap(self.map, self.stack_size);
                }
                set_stack_limit(self.old_stack_limit);
            }
        }

        fn _grow(stack_size: usize, f: &mut FnMut()) {
            let page_size = unsafe { getpagesize() } as usize;

            // Round the stack size up to a multiple of page_size
            let rem = stack_size % page_size;
            let stack_size = if rem == 0 {
                stack_size
            } else {
                stack_size.checked_add(page_size - rem)
                          .expect("stack size calculation overflowed")
            };

            // We need at least 2 page
            let stack_size = std::cmp::max(stack_size, page_size);

            // Add a guard page
            let stack_size = stack_size.checked_add(page_size)
                                       .expect("stack size calculation overflowed");

            // Allocate some new stack for ourselves
            let map = unsafe {
                libc::mmap(std::ptr::null_mut(),
                           stack_size,
                           libc::PROT_NONE,
                           libc::MAP_PRIVATE |
                           libc::MAP_ANON,
                           0,
                           0)
            };
            if map == -1isize as _ {
                panic!("unable to allocate stack")
            }
            let _switch = StackSwitch {
                map,
                stack_size,
                old_stack_limit: get_stack_limit(),
            };
            let result = unsafe {
                libc::mprotect((map as usize + page_size) as *mut libc::c_void,
                               stack_size - page_size,
                               libc::PROT_READ | libc::PROT_WRITE)
            };
            if result == -1 {
                panic!("unable to set stack permissions")
            }
            let stack_low = map as usize;

            // Prepare stack limits for the stack switch
            set_stack_limit(Some(stack_low));

            // Make sure the stack is 16-byte aligned which should be enough for all
            // platforms right now. Allocations on 64-bit are already 16-byte aligned
            // and our switching routine doesn't push any other data, but the routine on
            // 32-bit pushes an argument so we need a bit of an offset to get it 16-byte
            // aligned when the call is made.
            let offset = if cfg!(target_pointer_width = "32") {
                12
            } else {
                0
            };

            struct Context<'a> {
                closure: &'a mut FnMut(),
                panic: Option<Box<Any+Send>>,
            }

            extern fn doit(cx: &mut Context) {
                let closure = AssertUnwindSafe(&mut cx.closure);
                cx.panic = panic::catch_unwind(closure).err();
            }

            unsafe {
                let mut cx = Context {
                    closure: f,
                    panic: None,
                };
                __stacker_switch_stacks(&mut cx as *mut Context as *mut u8,
                                        doit as usize as *const _,
                                        stack_low + stack_size - offset);
                if let Some(panic) = cx.panic.take() {
                    panic::resume_unwind(panic);
                }
            }

            // Dropping `switch` frees the memory mapping and restores the old stack limit
        }
    }
}

cfg_if! {
    if #[cfg(windows)] {
        use std::ptr;
        use std::io;

        use winapi::shared::basetsd::*;
        use winapi::shared::minwindef::{LPVOID, BOOL};
        use winapi::shared::ntdef::*;
        use winapi::um::fibersapi::*;
        use winapi::um::memoryapi::*;
        use winapi::um::processthreadsapi::*;
        use winapi::um::winbase::*;

        extern {
            fn __stacker_get_current_fiber() -> PVOID;
        }

        struct FiberInfo<'a> {
            callback: &'a mut FnMut(),
            result: Option<std::thread::Result<()>>,
            parent_fiber: LPVOID,
        }

        unsafe extern "system" fn fiber_proc(info: LPVOID) {
            let info = &mut *(info as *mut FiberInfo);

            // Remember the old stack limit
            let old_stack_limit = get_stack_limit();
            // Update the limit to that of the fiber stack
            set_stack_limit(guess_os_stack_limit());

            info.result = Some(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (info.callback)();
            })));

            // Restore the stack limit of the previous fiber
            set_stack_limit(old_stack_limit);

            SwitchToFiber(info.parent_fiber);
            return;
        }

        fn _grow(stack_size: usize, callback: &mut FnMut()) {
            unsafe {
                // Fibers (or stackful coroutines) is the only way to create new stacks on the
                // same thread on Windows. So in order to extend the stack we create fiber
                // and switch to it so we can use it's stack. After running
                // `callback` we switch back to the current stack and destroy
                // the fiber and its associated stack.

                let was_fiber = IsThreadAFiber() == TRUE as BOOL;

                let mut info = FiberInfo {
                    callback,
                    result: None,

                    // We need a handle to the current stack / fiber so we can switch back to it
                    parent_fiber: {
                        // Is the current thread already a fiber? This is the case when we already
                        // used a fiber to extend the stack
                        if was_fiber {
                            // Get a handle to the current fiber. We need to use C for this
                            // as GetCurrentFiber is an header only function.
                            __stacker_get_current_fiber()
                        } else {
                            // Convert the current thread to a fiber, so we are able to switch back
                            // to the current stack. Threads coverted to fibers still act like
                            // regular threads, but they have associated fiber data. We later
                            // convert it back to a regular thread and free the fiber data.
                            ConvertThreadToFiber(ptr::null_mut())
                        }
                    },
                };
                if info.parent_fiber.is_null() {
                    // We don't have a handle to the fiber, so we can't switch back
                    panic!("unable to convert thread to fiber: {}", io::Error::last_os_error());
                }

                let fiber = CreateFiber(
                    stack_size as SIZE_T,
                    Some(fiber_proc),
                    &mut info as *mut FiberInfo as *mut _,
                );
                if fiber.is_null() {
                    panic!("unable to allocate fiber: {}", io::Error::last_os_error());
                }

                // Switch to the fiber we created. This changes stacks and starts executing
                // fiber_proc on it. fiber_proc will run `callback` and then switch back
                SwitchToFiber(fiber);

                // We are back on the old stack and now we have destroy the fiber and its stack
                DeleteFiber(fiber);

                // If we started out on a non-fiber thread, we converted that thread to a fiber.
                // Here we convert back.
                if !was_fiber {
                    if ConvertFiberToThread() == 0 {
                        panic!("unable to convert back to thread: {}", io::Error::last_os_error());
                    }
                }

                if let Err(payload) = info.result.unwrap() {
                    std::panic::resume_unwind(payload);
                }
            }
        }

        #[inline(always)]
        fn get_thread_stack_guarantee() -> usize {
            let min_guarantee = if cfg!(target_pointer_width = "32") {
                0x1000
            } else {
                0x2000
            };
            let mut stack_guarantee = 0;
            unsafe {
                // Read the current thread stack guarantee
                // This is the stack reserved for stack overflow
                // exception handling.
                // This doesn't return the true value so we need
                // some further logic to calculate the real stack
                // guarantee. This logic is what is used on x86-32 and
                // x86-64 Windows 10. Other versions and platforms may differ
                SetThreadStackGuarantee(&mut stack_guarantee)
            };
            std::cmp::max(stack_guarantee, min_guarantee) as usize + 0x1000
        }

        #[inline(always)]
        unsafe fn guess_os_stack_limit() -> Option<usize> {
            let mut mi = std::mem::zeroed();
            // Query the allocation which contains our stack pointer in order
            // to discover the size of the stack
            VirtualQuery(
                __stacker_stack_pointer() as *const _,
                &mut mi,
                std::mem::size_of_val(&mi) as SIZE_T,
            );
            Some(mi.AllocationBase as usize + get_thread_stack_guarantee() + 0x1000)
        }
    } else if #[cfg(target_os = "linux")] {
        use std::mem;

        unsafe fn guess_os_stack_limit() -> Option<usize> {
            let mut attr: libc::pthread_attr_t = mem::zeroed();
            assert_eq!(libc::pthread_attr_init(&mut attr), 0);
            assert_eq!(libc::pthread_getattr_np(libc::pthread_self(),
                                                &mut attr), 0);
            let mut stackaddr = 0 as *mut _;
            let mut stacksize = 0;
            assert_eq!(libc::pthread_attr_getstack(&attr, &mut stackaddr,
                                                   &mut stacksize), 0);
            assert_eq!(libc::pthread_attr_destroy(&mut attr), 0);
            Some(stackaddr as usize)
        }
    } else if #[cfg(target_os = "macos")] {
        unsafe fn guess_os_stack_limit() -> Option<usize> {
            Some(libc::pthread_get_stackaddr_np(libc::pthread_self()) as usize -
                libc::pthread_get_stacksize_np(libc::pthread_self()) as usize)
        }
    } else {
        // fallback for other platforms is to always increase the stack if we're on
        // the root stack. After we increased the stack once, we know the new stack
        // size and don't need this pessimization anymore
        #[inline(always)]
        unsafe fn guess_os_stack_limit() -> Option<usize> {
            None
        }
    }
}
