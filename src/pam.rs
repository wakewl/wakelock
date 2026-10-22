// PAM password check (libpam FFI).
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr,
};

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 19;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

#[repr(C)]
struct PamConv {
    conv: extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int,
    appdata_ptr: *mut c_void,
}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(service: *const c_char, user: *const c_char, conv: *const PamConv, pamh: *mut *mut c_void) -> c_int;
    fn pam_start_confdir(
        service: *const c_char,
        user: *const c_char,
        conv: *const PamConv,
        confdir: *const c_char,
        pamh: *mut *mut c_void,
    ) -> c_int;
    fn pam_authenticate(pamh: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(pamh: *mut c_void, status: c_int) -> c_int;
    fn pam_strerror(pamh: *mut c_void, errnum: c_int) -> *const c_char;
}

struct Conversation {
    password: CString,
    notice: String,
}

extern "C" fn converse(
    count: c_int,
    messages: *mut *const PamMessage,
    responses: *mut *mut PamResponse,
    data: *mut c_void,
) -> c_int {
    if count <= 0 || messages.is_null() || responses.is_null() || data.is_null() {
        return PAM_CONV_ERR;
    }
    let conversation = unsafe { &mut *(data as *mut Conversation) };
    // PAM frees the responses with free(), so they must come from malloc
    let replies = unsafe { libc::calloc(count as usize, std::mem::size_of::<PamResponse>()) } as *mut PamResponse;
    if replies.is_null() {
        return PAM_BUF_ERR;
    }
    for i in 0..count as usize {
        let message = unsafe { &**messages.add(i) };
        let text = if message.msg.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(message.msg) }.to_string_lossy().into_owned()
        };
        match message.msg_style {
            PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                let copy = unsafe { libc::strdup(conversation.password.as_ptr()) };
                if copy.is_null() {
                    for j in 0..i {
                        let resp = unsafe { (*replies.add(j)).resp };
                        if !resp.is_null() {
                            unsafe {
                                libc::explicit_bzero(resp as *mut c_void, libc::strlen(resp));
                                libc::free(resp as *mut c_void);
                            }
                        }
                    }
                    unsafe { libc::free(replies as *mut c_void) };
                    return PAM_BUF_ERR;
                }
                unsafe { (*replies.add(i)).resp = copy };
            }
            PAM_ERROR_MSG | PAM_TEXT_INFO
                if !text.trim().is_empty() => {
                    conversation.notice = text.trim().to_owned();
                }
            _ => {}
        }
    }
    unsafe { *responses = replies };
    PAM_SUCCESS
}

pub struct Outcome {
    pub success: bool,
    pub notice: String,
    /// PAM itself failed, rather than rejecting the password
    pub error: Option<String>,
}

pub fn authenticate(service: &str, confdir: Option<&str>, user: &str, password: String) -> Outcome {
    let fail = |error: String| Outcome { success: false, notice: String::new(), error: Some(error) };
    let (Ok(service_c), Ok(user_c)) = (CString::new(service), CString::new(user)) else {
        return fail("invalid PAM service or user name".into());
    };
    let mut bytes = password.into_bytes();
    let password = match CString::new(bytes.clone()) {
        Ok(p) => p,
        Err(_) => {
            wipe(&mut bytes);
            return Outcome { success: false, notice: String::new(), error: None };
        }
    };
    wipe(&mut bytes);
    let mut conversation = Box::new(Conversation { password, notice: String::new() });
    let conv = PamConv { conv: converse, appdata_ptr: &mut *conversation as *mut Conversation as *mut c_void };

    let mut handle: *mut c_void = ptr::null_mut();
    let started = match confdir.map(CString::new) {
        Some(Ok(dir)) => unsafe { pam_start_confdir(service_c.as_ptr(), user_c.as_ptr(), &conv, dir.as_ptr(), &mut handle) },
        Some(Err(_)) => return fail("invalid PAM directory".into()),
        None => unsafe { pam_start(service_c.as_ptr(), user_c.as_ptr(), &conv, &mut handle) },
    };
    if started != PAM_SUCCESS || handle.is_null() {
        return fail(format!("pam_start failed ({started})"));
    }
    let result = unsafe { pam_authenticate(handle, 0) };
    let error = match result {
        PAM_SUCCESS => None,
        // Wrong password
        6 | 7 | 8 | 10 | 11 => None,
        _ => Some(unsafe { CStr::from_ptr(pam_strerror(handle, result)) }.to_string_lossy().into_owned()),
    };
    unsafe { pam_end(handle, result) };

    let mut secret = std::mem::take(&mut conversation.password).into_bytes();
    wipe(&mut secret);
    Outcome { success: result == PAM_SUCCESS, notice: std::mem::take(&mut conversation.notice), error }
}

fn wipe(bytes: &mut [u8]) {
    unsafe { libc::explicit_bzero(bytes.as_mut_ptr() as *mut c_void, bytes.len()) };
}
