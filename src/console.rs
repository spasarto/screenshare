use std::ffi::c_void;

const GENERIC_WRITE: u32 = 0x40000000;
const FILE_SHARE_WRITE: u32 = 0x00000002;
const OPEN_EXISTING: u32 = 3;
const STD_OUTPUT_HANDLE: u32 = u32::MAX - 10; // (DWORD)-11
const STD_ERROR_HANDLE: u32 = u32::MAX - 11; // (DWORD)-12

#[link(name = "kernel32")]
extern "system" {
    fn AllocConsole() -> i32;
    fn FreeConsole() -> i32;
    fn GetConsoleWindow() -> *mut c_void;
    fn SetStdHandle(n_std_handle: u32, h_handle: *mut c_void) -> i32;
    fn CreateFileA(
        lp_file_name: *const u8,
        dw_desired_access: u32,
        dw_share_mode: u32,
        lp_security_attributes: *mut c_void,
        dw_creation_disposition: u32,
        dw_flags_and_attributes: u32,
        h_template_file: *mut c_void,
    ) -> *mut c_void;
}

pub fn toggle() {
    unsafe {
        let hwnd = GetConsoleWindow();
        if hwnd.is_null() {
            // No console — allocate one and redirect stdout/stderr into it.
            if AllocConsole() != 0 {
                let handle = CreateFileA(
                    c"CONOUT$".as_ptr().cast(),
                    GENERIC_WRITE,
                    FILE_SHARE_WRITE,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                );
                // INVALID_HANDLE_VALUE is -1isize cast to pointer
                if handle != (-1_isize as *mut c_void) {
                    SetStdHandle(STD_OUTPUT_HANDLE, handle);
                    SetStdHandle(STD_ERROR_HANDLE, handle);
                }
            }
        } else {
            FreeConsole();
        }
    }
}
