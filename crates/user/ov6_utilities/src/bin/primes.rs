#![no_std]

use ov6_user_lib::{
    error::Ov6Error,
    io::{Read as _, Write as _},
    pipe::{self, PipeReader},
    println,
    process::{self},
};
use ov6_utilities::{OrExit as _, exit, exit_err};

fn read_number(rx: &mut PipeReader) -> Option<u32> {
    let pid = process::id();
    let mut bytes = [0; 4];
    match rx.read_exact(&mut bytes) {
        Ok(()) => Some(u32::from_ne_bytes(bytes)),
        Err(Ov6Error::ReadExactEof) => None,
        Err(e) => exit_err!(e, "[?:{pid}] cannot read number"),
    }
}

fn sieve(mut rx: PipeReader) -> ! {
    loop {
        let pid = process::id();
        let Some(n) = read_number(&mut rx) else {
            process::exit(0);
        };
        println!("prime {n}");

        let (new_rx, mut tx) =
            pipe::pipe().or_exit(|e| exit_err!(e, "[{n}:{pid}] cannot create pipe"));
        let handle =
            process::fork().or_exit(|e| exit_err!(e, "[{n}:{pid}] cannot fork child process"));
        if handle.is_child() {
            drop(tx);
            rx = new_rx;
            continue;
        }
        drop(new_rx);

        loop {
            let Some(i) = read_number(&mut rx) else {
                break;
            };
            if i.is_multiple_of(n) {
                continue;
            }
            tx.write_all(&i.to_ne_bytes())
                .or_exit(|e| exit_err!(e, "[{n}:{pid}] cannot send bytes to child"));
        }
        drop(tx);

        let exit_status = handle
            .join()
            .or_exit(|e| exit_err!(e, "[{n}:{pid}] cannot join with child process"));

        if !exit_status.success() {
            exit!(
                "[{n}:{pid}] child process failed with {}",
                exit_status.code()
            );
        }
        process::exit(0);
    }
}

fn main() {
    const N: u32 = 280;

    let pid = process::id();
    let (rx, mut tx) = pipe::pipe().or_exit(|e| exit_err!(e, "[_:{pid}] cannot create pipe"));
    let Some(mut child) = process::fork()
        .or_exit(|e| exit_err!(e, "[_:{pid}] cannot fork child process"))
        .into_parent()
    else {
        drop(tx);
        sieve(rx);
    };
    drop(rx);
    for i in 2..N {
        tx.write_all(&i.to_ne_bytes())
            .or_exit(|e| exit_err!(e, "[_:{pid}] cannod send bytes to child"));
    }
    drop(tx);

    let exit_state = child
        .wait()
        .or_exit(|e| exit_err!(e, "[_:{pid}] cannot wait child process"));
    if !exit_state.success() {
        exit!("[_:{pid}] child process failed with {}", exit_state.code());
    }
}
