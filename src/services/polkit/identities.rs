use std::ffi::{CStr, c_char, c_int};
use std::ptr;

use crate::dbus::Value;

use super::Identity;

#[repr(C)]
struct Passwd {
    name: *mut c_char,
    password: *mut c_char,
    uid: u32,
    gid: u32,
    gecos: *mut c_char,
    directory: *mut c_char,
    shell: *mut c_char,
}

#[repr(C)]
struct Group {
    name: *mut c_char,
    password: *mut c_char,
    gid: u32,
    members: *mut *mut c_char,
}

// no passwd or group entry comes near this, and one that does answers with no entry instead
const BUFFER: usize = 16 * 1024;

#[link(name = "c")]
unsafe extern "C" {
    fn geteuid() -> u32;

    fn getpwuid_r(
        uid: u32,
        entry: *mut Passwd,
        buffer: *mut c_char,
        length: usize,
        result: *mut *mut Passwd,
    ) -> c_int;

    fn getgrgid_r(
        gid: u32,
        entry: *mut Group,
        buffer: *mut c_char,
        length: usize,
        result: *mut *mut Group,
    ) -> c_int;
}

pub(super) fn euid() -> u32 {
    unsafe { geteuid() }
}

// the users polkit says may answer, in the order it sent them and each one once
pub(super) fn users(identities: &Value) -> Vec<Identity> {
    let mut users = Vec::new();

    for identity in identities.list() {
        let [kind, details] = identity.list() else {
            continue;
        };

        match kind.text() {
            "unix-user" => {
                if let Some(name) = user(details.get("uid").number() as u32) {
                    add(&mut users, name);
                }
            }

            "unix-group" => {
                for name in members(details.get("gid").number() as u32) {
                    add(&mut users, name);
                }
            }

            _ => {}
        }
    }

    users
}

fn add(users: &mut Vec<Identity>, name: String) {
    if users.iter().any(|identity| identity.name == name) {
        return;
    }

    users.push(Identity { name });
}

// the name of the local user with this uid, and none for a uid no account carries
fn user(uid: u32) -> Option<String> {
    let mut buffer = vec![0u8; BUFFER];
    let mut entry: Passwd = unsafe { std::mem::zeroed() };
    let mut result = ptr::null_mut();

    let status = unsafe {
        getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };

    if status != 0 || result.is_null() {
        return None;
    }

    text(entry.name)
}

// the users listed as members of this group; one whose primary group it is stays unnamed here
fn members(gid: u32) -> Vec<String> {
    let mut buffer = vec![0u8; BUFFER];
    let mut entry: Group = unsafe { std::mem::zeroed() };
    let mut result = ptr::null_mut();

    let status = unsafe {
        getgrgid_r(
            gid,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };

    if status != 0 || result.is_null() || entry.members.is_null() {
        return Vec::new();
    }

    let mut names = Vec::new();
    let mut index = 0;

    loop {
        let member = unsafe { *entry.members.add(index) };

        if member.is_null() {
            return names;
        }

        if let Some(name) = text(member) {
            names.push(name);
        }

        index += 1;
    }
}

fn text(pointer: *mut c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }

    Some(
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned(),
    )
}
