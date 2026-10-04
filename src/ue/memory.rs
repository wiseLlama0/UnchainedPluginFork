use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Threading::GetCurrentProcess;

/// Copies a `T` from `ptr` without dereferencing it: ReadProcessMemory on our own process fails
/// cleanly on unmapped or protected memory where a plain read raises an access violation. For
/// values read through pointers that may be dangling or garbage (fields of game objects that
/// were never validated).
pub fn try_read<T: Copy>(ptr: *const T) -> Option<T> {
    if ptr.is_null() {
        return None;
    }
    let size = std::mem::size_of::<T>();
    let mut out = std::mem::MaybeUninit::<T>::uninit();
    let mut bytes_read = 0usize;
    let result = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            ptr.cast(),
            out.as_mut_ptr().cast(),
            size,
            Some(&mut bytes_read),
        )
    };
    (result.is_ok() && bytes_read == size).then(|| unsafe { out.assume_init() })
}
