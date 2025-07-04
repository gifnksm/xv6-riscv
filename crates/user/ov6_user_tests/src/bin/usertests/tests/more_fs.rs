use core::time::Duration;

use ov6_fs_types::FS_BLOCK_SIZE;
use ov6_kernel_params::{MAX_OP_BLOCKS, NINODE};
use ov6_user_lib::{
    env,
    error::Ov6Error,
    fs::{self, File},
    io::{Read as _, Write as _},
    os_str::OsStr,
    process::{self, ProcessBuilder},
    thread,
};
use ov6_user_tests::expect;
use safe_cast::to_u8;

use crate::{BUF, README_PATH, ROOT_DIR_PATH};

const TRUNC_FILE_PATH: &str = "truncfile";
const IPUTDIR_PATH: &str = "iputdir";
const OIDIR_PATH: &str = "oidir";

/// test `O_TRUNC`.
pub fn truncate1() {
    let mut buf = [0; 6];

    let _ = fs::remove_file(TRUNC_FILE_PATH);

    let mut file1 = File::create(TRUNC_FILE_PATH).unwrap();
    file1.write(b"abcd").unwrap();
    drop(file1);

    let mut file2 = File::open(TRUNC_FILE_PATH).unwrap();
    expect!(file2.read(&mut buf), Ok(4));
    assert_eq!(buf[0..4], *b"abcd");

    let mut file1 = File::options()
        .write(true)
        .truncate(true)
        .open(TRUNC_FILE_PATH)
        .unwrap();

    let mut file3 = File::open(TRUNC_FILE_PATH).unwrap();
    expect!(file3.read(&mut buf), Ok(0));

    expect!(file2.read(&mut buf), Ok(0));

    file1.write_all(b"efghij").unwrap();

    file3.read_exact(&mut buf).unwrap();
    assert_eq!(buf[0..6], *b"efghij");

    expect!(file2.read(&mut buf), Ok(2));
    assert_eq!(buf[0..2], *b"ij");

    fs::remove_file(TRUNC_FILE_PATH).unwrap();
    drop(file1);
    drop(file2);
    drop(file3);
}

/// write to an open FD whose file has just been truncated.
/// this causes a write at an offset beyond the end of the file.
/// such writes fail on ov6 (unlike POSIX) but at least
/// they don't crash.
pub fn truncate2() {
    let mut file1 = File::create(TRUNC_FILE_PATH).unwrap();
    file1.write_all(b"abcd").unwrap();

    let _file2 = File::options()
        .write(true)
        .truncate(true)
        .open(TRUNC_FILE_PATH)
        .unwrap();

    expect!(file1.write(b"x"), Err(Ov6Error::NotSeekable));

    fs::remove_file(TRUNC_FILE_PATH).unwrap();
}

pub fn truncate3() {
    drop(File::create(TRUNC_FILE_PATH).unwrap());

    let mut buf = [0; 32];

    let mut child = ProcessBuilder::new()
        .spawn_fn(|| {
            for _i in 0..100 {
                let mut file = File::options().write(true).open(TRUNC_FILE_PATH).unwrap();
                file.write_all(b"1234567890").unwrap();
                drop(file);
                let mut file = File::open(TRUNC_FILE_PATH).unwrap();
                file.read(&mut buf).unwrap();
                drop(file);
            }

            process::exit(0);
        })
        .unwrap();

    for _i in 0..150 {
        let mut file = File::create(TRUNC_FILE_PATH).unwrap();
        file.write_all(b"xxx").unwrap();
        drop(file);
    }

    let status = child.wait().unwrap();
    assert!(status.success());

    fs::remove_file(TRUNC_FILE_PATH).unwrap();
}

/// does the error path in `open()` for attempt to write a
/// directory call `Inode::put()` in a transaction?
/// needs a hacked kernel that pauses just after the `namei()`
/// call in `sys_open()`:
///
/// ```c
/// if((ip = namei(path)) == 0)
///   return -1;
/// {
///   int i;
///   for(i = 0; i < 10000; i++)
///     yield();
/// }
/// ```
pub fn inode_put_open() {
    fs::create_dir(OIDIR_PATH).unwrap();

    let mut child = ProcessBuilder::new()
        .spawn_fn(|| {
            expect!(
                File::options().read(true).write(true).open(OIDIR_PATH),
                Err(Ov6Error::IsADirectory),
            );
            process::exit(0);
        })
        .unwrap();

    thread::sleep(Duration::from_millis(100));
    fs::remove_file(OIDIR_PATH).unwrap();

    let status = child.wait().unwrap();
    assert!(status.success());
}

/// does `exit()` call `Inode::put(p->cwd)` in a transaction?
pub fn inode_put_exit() {
    let status = ProcessBuilder::new()
        .spawn_fn(|| {
            fs::create_dir(IPUTDIR_PATH).unwrap();
            env::set_current_directory(IPUTDIR_PATH).unwrap();
            fs::remove_file("../iputdir").unwrap();
            process::exit(0);
        })
        .unwrap()
        .wait()
        .unwrap();
    assert!(status.success());
}

/// does `chdir()` call `Inode::put(p->cwd)` in a transaction?
pub fn inode_put_chdir() {
    fs::create_dir(IPUTDIR_PATH).unwrap();
    env::set_current_directory(IPUTDIR_PATH).unwrap();
    fs::remove_file("../iputdir").unwrap();
    env::set_current_directory(ROOT_DIR_PATH).unwrap();
}

/// two processes write to the same file descriptor
/// is the offset shared? does inode locking work?
pub fn shared_fd() {
    const FILE_PATH: &str = "sharedfd";
    const N: usize = 1000;
    const SIZE: usize = 10;

    let mut buf = [0; SIZE];

    let _ = fs::remove_file(FILE_PATH);
    let mut file = File::create(FILE_PATH).unwrap();
    let handle = process::fork().unwrap();
    buf.fill(if handle.is_child() { b'c' } else { b'p' });
    for _ in 0..N {
        file.write_all(&buf).unwrap();
    }
    assert!(handle.join().unwrap().success());
    drop(file);

    let mut file = File::open(FILE_PATH).unwrap();
    let mut buf = [0; SIZE];
    let mut nc = 0;
    let mut np = 0;

    loop {
        let n = file.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        assert_eq!(n, SIZE);
        for &c in &buf {
            if c == b'c' {
                nc += 1;
            } else if c == b'p' {
                np += 1;
            } else {
                panic!("unexpected char");
            }
        }
    }
    drop(file);
    fs::remove_file(FILE_PATH).unwrap();
    assert_eq!(nc, N * SIZE);
    assert_eq!(np, N * SIZE);
}

pub fn four_files() {
    const N: usize = 12;
    const SIZE: usize = 500;

    let buf = unsafe { (&raw mut BUF).as_mut() }.unwrap();
    let buf = &mut buf[..SIZE];

    let names = ["f0", "f1", "f2", "f3"];
    let bytes = [b'0', b'1', b'2', b'3'];
    for (&name, &b) in names.iter().zip(&bytes) {
        let _ = fs::remove_file(name);
        ProcessBuilder::new()
            .spawn_fn(|| {
                let mut file = File::create(name).unwrap();
                buf.fill(b);
                for _ in 0..N {
                    file.write_all(buf).unwrap();
                }
                process::exit(0);
            })
            .unwrap();
    }

    for _ in 0..names.len() {
        let (_, status) = process::wait_any().unwrap();
        assert!(status.success());
    }

    for (&name, &b) in names.iter().zip(&bytes) {
        let mut file = File::open(name).unwrap();
        let mut total = 0;
        loop {
            let n = file.read(buf).unwrap();
            if n == 0 {
                break;
            }
            for &c in buf.iter().take(n) {
                assert_eq!(c, b);
            }
            total += n;
        }
        drop(file);
        assert_eq!(total, N * SIZE);
        fs::remove_file(name).unwrap();
    }
}

/// four processes create and delete different files in same directory
pub fn create_delete() {
    const N: usize = 20;
    const NCHILD: usize = 4;
    let mut name = [0; 2];

    for pi in 0..NCHILD {
        ProcessBuilder::new()
            .spawn_fn(|| {
                name[0] = b'p' + u8::try_from(pi).unwrap();
                for i in 0..N {
                    name[1] = b'0' + u8::try_from(i).unwrap();
                    let path = OsStr::from_bytes(&name);
                    let file = File::create(path).unwrap();
                    drop(file);
                    if i > 0 && i.is_multiple_of(2) {
                        name[1] = b'0' + u8::try_from(i / 2).unwrap();
                        let path = OsStr::from_bytes(&name);
                        fs::remove_file(path).unwrap();
                    }
                }
                process::exit(0);
            })
            .unwrap();
    }

    for _ in 0..NCHILD {
        let (_, status) = process::wait_any().unwrap();
        assert!(status.success());
    }

    for i in 0..N {
        for pi in 0..NCHILD {
            name[0] = b'p' + u8::try_from(pi).unwrap();
            name[1] = b'0' + u8::try_from(i).unwrap();
            let path = OsStr::from_bytes(&name);
            let file = File::open(path);
            assert!(
                (1..N / 2).contains(&i) || file.is_ok(),
                "oops create_delete {} didn't exist",
                path.display(),
            );
            assert!(
                !((1..N / 2).contains(&i) && file.is_ok()),
                "oops create_delete {} did exist",
                path.display(),
            );
        }
    }

    for i in 0..N {
        for pi in 0..NCHILD {
            name[0] = b'p' + u8::try_from(pi).unwrap();
            name[1] = b'0' + u8::try_from(i).unwrap();
            let path = OsStr::from_bytes(&name);
            let _ = fs::remove_file(path);
        }
    }
}

/// can I unlink a file and still read it?
pub fn unlink_read() {
    const FILE_PATH: &str = "unlinkread";

    File::create(FILE_PATH)
        .unwrap()
        .write_all(b"hello")
        .unwrap();

    let mut file1 = File::options()
        .create(true)
        .read(true)
        .write(true)
        .open(FILE_PATH)
        .unwrap();
    fs::remove_file(FILE_PATH).unwrap();

    let mut file2 = File::create(FILE_PATH).unwrap();
    file2.write_all(b"yyy").unwrap();
    drop(file2);

    let mut buf = [0; 5];
    file1.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello");
    file1.write_all(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
    drop(file1);

    fs::remove_file(FILE_PATH).unwrap();
}

pub fn link() {
    const FILE1_PATH: &str = "lf1";
    const FILE2_PATH: &str = "lf2";

    let mut file = File::create(FILE1_PATH).unwrap();
    file.write_all(b"hello").unwrap();
    drop(file);

    fs::link(FILE1_PATH, FILE2_PATH).unwrap();
    fs::remove_file(FILE1_PATH).unwrap();

    expect!(File::open(FILE1_PATH), Err(Ov6Error::FsEntryNotFound));

    let mut file = File::open(FILE2_PATH).unwrap();
    let mut buf = [0; 5];
    file.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello");
    drop(file);

    expect!(
        fs::link(FILE2_PATH, FILE2_PATH),
        Err(Ov6Error::AlreadyExists)
    );

    fs::remove_file(FILE2_PATH).unwrap();
    expect!(
        fs::link(FILE2_PATH, FILE1_PATH),
        Err(Ov6Error::FsEntryNotFound)
    );

    expect!(fs::link(".", FILE1_PATH), Err(Ov6Error::NotADirectory));
}

/// test concurrent create/link/unlink of the same file
pub fn concreate() {
    const N: usize = 40;

    let mut file = [0; 2];
    file[0] = b'C';
    for i in 0..N {
        file[1] = b'0' + u8::try_from(i).unwrap();
        let path = OsStr::from_bytes(&file);
        let _ = fs::remove_file(path);

        let handle = process::fork().unwrap();
        if (handle.is_parent() && (i % 3) == 1) || (handle.is_child() && (i % 5) == 1) {
            let _ = fs::link("C0", path);
        } else {
            let file = File::create(path).unwrap();
            drop(file);
        }
        assert!(handle.join().unwrap().success());
    }

    let mut n = 0;
    let mut fa = [0; N];
    let dir = fs::read_dir(".").unwrap();
    for entry in dir {
        let entry = entry.unwrap();
        if entry.name().len() == 2 && entry.name().as_bytes()[0] == b'C' {
            let i = usize::from(entry.name().as_bytes()[1] - b'0');
            assert!(fa[i] == 0);
            fa[i] = 1;
            n += 1;
        }
    }
    assert_eq!(n, N);

    for i in 0..N {
        file[1] = b'0' + u8::try_from(i).unwrap();
        let path = OsStr::from_bytes(&file);

        let handle = process::fork().unwrap();
        if (i.is_multiple_of(3) && handle.is_child()) || ((i % 3) == 1 && handle.is_parent()) {
            let _ = File::open(path);
            let _ = File::open(path);
            let _ = File::open(path);
            let _ = File::open(path);
            let _ = File::open(path);
            let _ = File::open(path);
        } else {
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(path);
        }
        assert!(handle.join().unwrap().success());
    }
}

/// another concurrent link/unlink/create test,
/// to look for deadlocks.
pub fn link_unlink() {
    let _ = fs::remove_file("x");

    let handle = process::fork().unwrap();

    let mut x: u32 = if handle.is_parent() { 1 } else { 97 };
    for _ in 0..100 {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        match x % 3 {
            0 => {
                let _ = File::options().create(true).open("x");
            }
            1 => {
                let _ = fs::link("cat", "x");
            }
            _ => {
                let _ = fs::remove_file("x");
            }
        }
    }

    assert!(handle.join().unwrap().success());
    let _ = fs::remove_file("x");
}

pub fn subdir() {
    let _ = fs::remove_file("ff");
    fs::create_dir("dd").unwrap();

    let mut file = File::create("dd/ff").unwrap();
    file.write_all(b"ff").unwrap();
    drop(file);

    expect!(fs::remove_file("dd"), Err(Ov6Error::DirectoryNotEmpty));

    fs::create_dir("/dd/dd").unwrap();

    let mut file = File::create("dd/dd/ff").unwrap();
    file.write_all(b"FF").unwrap();
    drop(file);

    let mut file = File::open("dd/dd/../ff").unwrap();
    let mut buf = [0; 2];
    file.read_exact(&mut buf).unwrap();
    assert_eq!(buf, *b"ff");
    drop(file);

    fs::link("dd/dd/ff", "dd/dd/ffff").unwrap();
    fs::remove_file("dd/dd/ff").unwrap();
    expect!(File::open("dd/dd/ff"), Err(Ov6Error::FsEntryNotFound));

    env::set_current_directory("dd").unwrap();
    env::set_current_directory("dd/../../dd").unwrap();
    env::set_current_directory("dd/../../../dd").unwrap();
    env::set_current_directory("./..").unwrap();

    let mut file = File::open("dd/dd/ffff").unwrap();
    let mut buf = [0; 2];
    file.read_exact(&mut buf).unwrap();
    drop(file);

    expect!(File::open("dd/dd/ff"), Err(Ov6Error::FsEntryNotFound));

    expect!(File::create("dd/ff/ff"), Err(Ov6Error::NotADirectory));
    expect!(File::create("dd/xx/ff"), Err(Ov6Error::FsEntryNotFound));
    expect!(
        File::options().create(true).read(true).open("dd"),
        Err(Ov6Error::AlreadyExists)
    );
    expect!(
        File::options().read(true).write(true).open("dd"),
        Err(Ov6Error::IsADirectory)
    );
    expect!(
        File::options().write(true).open("dd"),
        Err(Ov6Error::IsADirectory)
    );
    expect!(
        fs::link("dd/ff/ff", "dd/dd/xx"),
        Err(Ov6Error::NotADirectory)
    );
    expect!(
        fs::link("dd/xx/ff", "dd/dd/xx"),
        Err(Ov6Error::FsEntryNotFound)
    );
    expect!(
        fs::link("dd/ff", "dd/dd/ffff"),
        Err(Ov6Error::AlreadyExists)
    );
    expect!(fs::create_dir("dd/ff/ff"), Err(Ov6Error::NotADirectory));
    expect!(fs::create_dir("dd/xx/ff"), Err(Ov6Error::FsEntryNotFound));
    expect!(fs::create_dir("dd/dd/ffff"), Err(Ov6Error::AlreadyExists));
    expect!(fs::remove_file("dd/xx/ff"), Err(Ov6Error::FsEntryNotFound));
    expect!(fs::remove_file("dd/ff/ff"), Err(Ov6Error::NotADirectory));
    expect!(
        env::set_current_directory("dd/ff"),
        Err(Ov6Error::NotADirectory)
    );
    expect!(
        env::set_current_directory("dd/xx"),
        Err(Ov6Error::FsEntryNotFound)
    );

    fs::remove_file("dd/dd/ffff").unwrap();
    fs::remove_file("dd/ff").unwrap();
    expect!(fs::remove_file("dd"), Err(Ov6Error::DirectoryNotEmpty));
    fs::remove_file("dd/dd").unwrap();
    fs::remove_file("dd").unwrap();
}

/// test writes that are larger than the log.
pub fn big_write() {
    const FILE_PATH: &str = "bigwrite";
    let buf = unsafe { (&raw mut BUF).as_mut() }.unwrap();

    let _ = fs::remove_file(FILE_PATH);

    for size in (499..(MAX_OP_BLOCKS + 2) * FS_BLOCK_SIZE).step_by(471) {
        let mut file = File::create(FILE_PATH).unwrap();
        for _ in 0..2 {
            file.write_all(&buf[..size]).unwrap();
        }
        drop(file);
        fs::remove_file(FILE_PATH).unwrap();
    }
}

pub fn big_file() {
    const FILE_PATH: &str = "bigfile.dat";
    const N: usize = 20;
    const SIZE: usize = 600;

    let buf = unsafe { (&raw mut BUF).as_mut() }.unwrap();
    let buf = &mut buf[..SIZE];

    let _ = fs::remove_file(FILE_PATH);
    let mut file = File::create(FILE_PATH).unwrap();
    for i in 0..to_u8!(N) {
        buf.fill(i);
        file.write(buf).unwrap();
    }
    drop(file);

    let mut file = File::open(FILE_PATH).unwrap();
    let mut total = 0;
    for i in 0.. {
        let n = file.read(&mut buf[..SIZE / 2]).unwrap();
        if n == 0 {
            break;
        }
        assert_eq!(n, SIZE / 2);
        assert!(buf[0] == i / 2 && buf[SIZE / 2 - 1] == i / 2);
        total += n;
    }
    drop(file);
    assert_eq!(total, N * SIZE);
    fs::remove_file(FILE_PATH).unwrap();
}

pub fn fourteen() {
    // DIR_SIZE is 14

    const N14: &str = "12345678901234";
    const N14_15: &str = "12345678901234/123456789012345";
    const N15_15_15: &str = "123456789012345/123456789012345/123456789012345";
    const N14_14_14: &str = "12345678901234/12345678901234/12345678901234";
    const N14_14: &str = "12345678901234/12345678901234";
    const N15_14: &str = "123456789012345/12345678901234";

    fs::create_dir(N14).unwrap();
    fs::create_dir(N14_15).unwrap();
    let _ = File::create(N15_15_15).unwrap();
    let _ = File::open(N14_14_14).unwrap();
    expect!(fs::create_dir(N14_14), Err(Ov6Error::AlreadyExists));
    expect!(fs::create_dir(N15_14), Err(Ov6Error::AlreadyExists));

    // clean up
    expect!(fs::remove_file(N15_14), Err(Ov6Error::DirectoryNotEmpty));
    expect!(fs::remove_file(N14_14), Err(Ov6Error::DirectoryNotEmpty));
    fs::remove_file(N14_14_14).unwrap();
    expect!(fs::remove_file(N15_15_15), Err(Ov6Error::FsEntryNotFound));
    fs::remove_file(N14_15).unwrap();
    fs::remove_file(N14).unwrap();
}

pub fn rm_dot() {
    fs::create_dir("dots").unwrap();
    env::set_current_directory("dots").unwrap();
    expect!(fs::remove_file("."), Err(Ov6Error::InvalidInput));
    expect!(fs::remove_file(".."), Err(Ov6Error::InvalidInput));

    env::set_current_directory("/").unwrap();
    expect!(fs::remove_file("dots/."), Err(Ov6Error::InvalidInput));
    expect!(fs::remove_file("dots/.."), Err(Ov6Error::InvalidInput));
    fs::remove_file("dots").unwrap();
}

pub fn dir_file() {
    let _file = File::create("dirfile").unwrap();
    expect!(
        env::set_current_directory("dirfile"),
        Err(Ov6Error::NotADirectory)
    );
    expect!(File::open("dirfile/xx"), Err(Ov6Error::NotADirectory));
    expect!(File::create("dirfile/xx"), Err(Ov6Error::NotADirectory));
    expect!(fs::create_dir("dirfile/xx"), Err(Ov6Error::NotADirectory));
    expect!(fs::remove_file("dirfile/xx"), Err(Ov6Error::NotADirectory));
    expect!(
        fs::link(README_PATH, "dirfile/xx"),
        Err(Ov6Error::NotADirectory)
    );
    fs::remove_file("dirfile").unwrap();

    expect!(
        File::options().read(true).write(true).open("."),
        Err(Ov6Error::IsADirectory),
    );
    let mut file = File::open(".").unwrap();
    expect!(file.write(b"x"), Err(Ov6Error::BadFileDescriptor));
}

/// test that `inode_put()` is called at the end of `_namei()`.
/// also tests empty file names.
pub fn iref() {
    const DIR_PATH: &str = "irefd";
    for _ in 0..=NINODE {
        fs::create_dir(DIR_PATH).unwrap();
        env::set_current_directory(DIR_PATH).unwrap();

        expect!(fs::create_dir(""), Err(Ov6Error::AlreadyExists));
        expect!(fs::link("README", ""), Err(Ov6Error::AlreadyExists));
        let _ = File::open("").unwrap();
        let _file = File::create("xx").unwrap();
        fs::remove_file("xx").unwrap();
    }

    // clean up
    for _ in 0..=NINODE {
        env::set_current_directory("..").unwrap();
        fs::remove_file(DIR_PATH).unwrap();
    }

    env::set_current_directory("/").unwrap();
}
