pub use anyhow::Result;
pub use inventory::submit as inventory_submit;
pub use phd_framework;
pub use phd_testcase_macros::phd_testcase;

use phd_framework::test_vm::factory::VmFactory;

/// The outcome from executing a specific test case.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TestOutcome {
    /// The test passed.
    Passed,

    /// The test failed; the payload is an optional failure message.
    Failed(Option<String>),

    /// The test chose to be skipped, i.e. it detected a parameter or condition
    /// that makes it impossible to execute the test or to meaningfully provide
    /// a pass/fail outcome. The payload is an optional message.
    Skipped(Option<String>),
}

/// The test context structure passed to every PHD test case.
pub struct TestContext {
    pub vm_factory: VmFactory,
}

/// A wrapper for test functions. This is needed to allow [`TestCase`] to have a
/// `const` constructor for the inventory crate.
pub struct TestFunction {
    pub f: fn(&TestContext) -> TestOutcome,
}

/// A description of a single test case.
pub struct TestCase {
    /// The path to the module containing the test case. This is generally
    /// derived from the `module_path!()` macro, which the `#[phd_testcase]`
    /// attribute macro uses when constructing the test case's inventory entry.
    pub(crate) module_path: &'static str,

    /// The name of this test case, which is generally its function name.
    pub(crate) name: &'static str,

    /// The test function to execute to run this test.
    pub(crate) function: TestFunction,
}

#[allow(dead_code)]
impl TestCase {
    /// Constructs a new [`TestCase`].
    pub const fn new(
        module_path: &'static str,
        name: &'static str,
        function: TestFunction,
    ) -> Self {
        Self { module_path, name, function }
    }

    /// Returns the test case's fully qualified name, i.e. `module_path::name`.
    pub fn fully_qualified_name(&self) -> String {
        format!("{}::{}", self.module_path, self.name)
    }

    /// Returns the test case's name.
    pub fn name(&self) -> &str {
        self.name
    }

    /// Runs the test case's body with the supplied test context and returns its
    /// outcome.
    pub fn run(&self, ctx: &TestContext) -> TestOutcome {
        (self.function.f)(ctx)
    }
}

inventory::collect!(TestCase);

pub fn all_test_cases() -> inventory::iter<TestCase> {
    inventory::iter::<TestCase>
}
