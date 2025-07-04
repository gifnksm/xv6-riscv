extern crate alloc;

use alloc::{borrow::ToOwned as _, string::String, vec, vec::Vec};

use ov6_user_lib::{
    env, eprint, eprintln,
    os::ov6::syscall,
    process::{self, ProcessBuilder},
    time::Instant,
};

use crate::message;

#[derive(Debug)]
pub enum TestError {
    TestFailed,
}

pub type TestFn = fn();

pub struct TestEntry {
    pub name: &'static str,
    pub test: TestFn,
    pub tags: &'static [&'static str],
}

impl TestEntry {
    fn run(&self) -> Result<(), TestError> {
        eprint!("{:-30} ", self.name);

        let start = Instant::now();

        let status = ProcessBuilder::new()
            .spawn_fn(|| {
                (self.test)();
                process::exit(0);
            })
            .unwrap()
            .wait()
            .unwrap();

        let elapsed = start.elapsed();

        if !status.success() {
            eprintln!(
                "FAIL [{:3}.{:03}s]",
                elapsed.as_secs(),
                elapsed.subsec_millis()
            );
            return Err(TestError::TestFailed);
        }

        eprintln!(
            "PASS [{:3}.{:03}s]",
            elapsed.as_secs(),
            elapsed.subsec_millis()
        );
        Ok(())
    }
}

fn get_free_pages() -> usize {
    let sysinfo = syscall::get_system_info().unwrap();
    sysinfo.memory.free_pages
}

enum TestNameMatchType {
    Exact,
    Beginning,
}

impl TestNameMatchType {
    fn matches(&self, test_name: &str, filter_name: &str) -> bool {
        match self {
            Self::Exact => test_name == filter_name,
            Self::Beginning => test_name.starts_with(filter_name),
        }
    }
}

struct TestFilter {
    name: Option<String>,
    name_match_type: TestNameMatchType,
    tags: Vec<String>,
}

impl TestFilter {
    fn matches(&self, entry: &TestEntry) -> bool {
        if let Some(filter_name) = &self.name
            && !self.name_match_type.matches(entry.name, filter_name)
        {
            return false;
        }

        if !self.tags.is_empty() && !self.tags.iter().any(|t| entry.tags.contains(&t.as_str())) {
            return false;
        }

        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Continuous {
    Once,
    UntilFailure,
    Forever,
}

impl Continuous {
    fn judge_result<E>(self, result: Result<(), E>) -> Result<(), E> {
        match self {
            Self::Once | Self::UntilFailure => result,
            Self::Forever => Ok(()),
        }
    }
}

pub struct TestParam {
    filter: TestFilter,
    continuous: Continuous,
    shutdown_after_tests: bool,
}

const USAGE_ARGS: &str = "[-c] [-C] [-q] [-b] [-T] [-t tag] [-h] [testname]";

impl TestParam {
    pub fn usage_and_exit() -> ! {
        eprintln!("Usage: {} {USAGE_ARGS}", env::arg0().display());
        process::exit(1);
    }

    fn help_and_exit() -> ! {
        eprintln!("Usage: {} {USAGE_ARGS}", env::arg0().display());
        eprintln!("    -c          Run tests continuously until a failure");
        eprintln!("    -C          Run tests continuously forever");
        eprintln!("    -b          Beginning matching test name");
        eprintln!("    -t          Run only tests with the given tags");
        eprintln!("    -T          Shutdown or abort after running tests");
        eprintln!("    -h          Print this help message");
        eprintln!("    testname    Run only the test with the given name");
        process::exit(1);
    }

    #[must_use]
    pub fn parse() -> Self {
        let mut args = env::args();
        let _ = args.next(); // skip the program name

        let mut param = Self {
            filter: TestFilter {
                name: None,
                name_match_type: TestNameMatchType::Exact,
                tags: vec![],
            },
            continuous: Continuous::Once,
            shutdown_after_tests: false,
        };

        while let Some(arg) = args.next() {
            match arg {
                "-c" => param.continuous = Continuous::UntilFailure,
                "-C" => param.continuous = Continuous::Forever,
                "-b" => param.filter.name_match_type = TestNameMatchType::Beginning,
                "-t" => {
                    let tag = args.next().unwrap_or_else(|| {
                        eprintln!("Missing argument for -t");
                        Self::usage_and_exit();
                    });
                    param.filter.tags.push(tag.to_owned());
                }
                "-T" => param.shutdown_after_tests = true,
                "-h" => Self::help_and_exit(),
                "--" => break,
                _ if !arg.starts_with('-') => param.filter.name = Some(arg.to_owned()),
                _ => Self::usage_and_exit(),
            }
        }

        param
    }

    fn run_tests(&self, run_count: &mut usize, tests: &[TestEntry]) -> Result<(), TestError> {
        for entry in tests {
            if !self.filter.matches(entry) {
                continue;
            }

            *run_count += 1;

            if let Err(e) = self.continuous.judge_result(entry.run()) {
                eprintln!("SOME TESTS FAILED");
                return Err(e);
            }
        }

        Ok(())
    }

    fn drive_tests(&self, run_count: &mut usize, tests: &[TestEntry]) -> Result<(), TestError> {
        loop {
            eprint!("freepages: ");
            let start = Instant::now();
            let free0 = get_free_pages();
            let elapsed = start.elapsed();
            eprintln!(
                "{free0} [{:3}.{:03}s]",
                elapsed.as_secs(),
                elapsed.subsec_millis()
            );

            eprintln!("starting");

            self.continuous
                .judge_result(self.run_tests(run_count, tests))?;

            eprint!("freepages: ");
            let start = Instant::now();
            let free1 = get_free_pages();
            let elapsed = start.elapsed();
            eprintln!(
                "{free1} [{:3}.{:03}s]",
                elapsed.as_secs(),
                elapsed.subsec_millis()
            );

            if free0 != free1 {
                eprintln!("freepages: FAIL -- lost some free pages {free1} (out of {free0})");
                return Err(TestError::TestFailed);
            }

            eprintln!("freepages: PASS");

            if self.continuous == Continuous::Once {
                break;
            }
        }

        Ok(())
    }

    /// Run the tests with the given parameters.
    ///
    /// # Panics
    ///
    /// Panics if halt or abort syscall fails.
    pub fn run(&self, tests: &[TestEntry]) -> ! {
        let match_count = tests
            .iter()
            .filter(|entry| self.filter.matches(entry))
            .count();
        if match_count == 0 {
            eprintln!("No tests matched the filter");
            process::exit(1);
        }

        let mut run_count = 0;

        let start = Instant::now();
        let res = self.drive_tests(&mut run_count, tests);
        let elapsed = start.elapsed();

        match res {
            Ok(()) if run_count > 0 => {
                message!(
                    "ALL TESTS PASSED [{:3}.{:03}s]",
                    elapsed.as_secs(),
                    elapsed.subsec_millis(),
                );
                if self.shutdown_after_tests {
                    syscall::halt(0).unwrap();
                }
                process::exit(0);
            }
            Ok(()) | Err(TestError::TestFailed) => {
                message!(
                    "TEST FAILED [{:3}.{:03}s]",
                    elapsed.as_secs(),
                    elapsed.subsec_millis()
                );
                if run_count == 0 {
                    message!("no tests runnned");
                }
                if self.shutdown_after_tests {
                    syscall::abort(1).unwrap();
                }
                process::exit(1);
            }
        }
    }
}
