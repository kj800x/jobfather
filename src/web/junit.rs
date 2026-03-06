use maud::{html, Markup};
use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Default)]
pub struct TestSuites {
    pub suites: Vec<TestSuite>,
}

#[derive(Default)]
pub struct TestSuite {
    pub name: String,
    pub tests: u32,
    pub failures: u32,
    pub errors: u32,
    pub skipped: u32,
    pub time: Option<String>,
    pub cases: Vec<TestCase>,
}

pub struct TestCase {
    pub name: String,
    pub classname: Option<String>,
    pub time: Option<String>,
    pub status: TestCaseStatus,
}

pub enum TestCaseStatus {
    Passed,
    Failed { message: Option<String>, body: Option<String> },
    Error { message: Option<String>, body: Option<String> },
    Skipped { message: Option<String> },
}

impl TestSuites {
    pub fn total_tests(&self) -> u32 {
        self.suites.iter().map(|s| s.tests).sum()
    }

    pub fn total_failures(&self) -> u32 {
        self.suites.iter().map(|s| s.failures).sum()
    }

    pub fn total_errors(&self) -> u32 {
        self.suites.iter().map(|s| s.errors).sum()
    }

    pub fn total_skipped(&self) -> u32 {
        self.suites.iter().map(|s| s.skipped).sum()
    }

    pub fn total_passed(&self) -> u32 {
        self.total_tests() - self.total_failures() - self.total_errors() - self.total_skipped()
    }

    pub fn all_passed(&self) -> bool {
        self.total_failures() == 0 && self.total_errors() == 0
    }
}

pub fn parse_junit_xml(xml: &str) -> Option<TestSuites> {
    let mut reader = Reader::from_str(xml);
    let mut result = TestSuites::default();
    let mut current_suite: Option<TestSuite> = None;
    let mut current_case: Option<TestCase> = None;
    let mut in_failure = false;
    let mut in_error = false;
    let mut text_buf = String::new();

    loop {
        let event = reader.read_event();
        let (e, is_empty) = match &event {
            Ok(Event::Start(e)) => (e, false),
            Ok(Event::Empty(e)) => (e, true),
            Ok(Event::Text(t)) => {
                if in_failure || in_error {
                    if let Ok(txt) = t.unescape() {
                        text_buf.push_str(&txt);
                    }
                }
                continue;
            }
            Ok(Event::End(e)) => {
                match e.local_name().as_ref() {
                    b"failure" => {
                        if let Some(ref mut tc) = current_case {
                            if let TestCaseStatus::Failed { ref mut body, .. } = tc.status {
                                if !text_buf.is_empty() {
                                    *body = Some(text_buf.clone());
                                }
                            }
                        }
                        in_failure = false;
                    }
                    b"error" => {
                        if let Some(ref mut tc) = current_case {
                            if let TestCaseStatus::Error { ref mut body, .. } = tc.status {
                                if !text_buf.is_empty() {
                                    *body = Some(text_buf.clone());
                                }
                            }
                        }
                        in_error = false;
                    }
                    b"testcase" => {
                        if let Some(tc) = current_case.take() {
                            if let Some(ref mut suite) = current_suite {
                                suite.cases.push(tc);
                            }
                        }
                    }
                    b"testsuite" => {
                        if let Some(suite) = current_suite.take() {
                            result.suites.push(suite);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => continue,
        };

        match e.local_name().as_ref() {
            b"testsuite" => {
                let mut suite = TestSuite::default();
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"name" => suite.name = String::from_utf8_lossy(&attr.value).into(),
                        b"tests" => suite.tests = String::from_utf8_lossy(&attr.value).parse().unwrap_or(0),
                        b"failures" => suite.failures = String::from_utf8_lossy(&attr.value).parse().unwrap_or(0),
                        b"errors" => suite.errors = String::from_utf8_lossy(&attr.value).parse().unwrap_or(0),
                        b"skipped" => suite.skipped = String::from_utf8_lossy(&attr.value).parse().unwrap_or(0),
                        b"time" => suite.time = Some(String::from_utf8_lossy(&attr.value).into()),
                        _ => {}
                    }
                }
                current_suite = Some(suite);
            }
            b"testcase" => {
                let mut tc = TestCase {
                    name: String::new(),
                    classname: None,
                    time: None,
                    status: TestCaseStatus::Passed,
                };
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"name" => tc.name = String::from_utf8_lossy(&attr.value).into(),
                        b"classname" => tc.classname = Some(String::from_utf8_lossy(&attr.value).into()),
                        b"time" => tc.time = Some(String::from_utf8_lossy(&attr.value).into()),
                        _ => {}
                    }
                }
                if is_empty {
                    // Self-closing <testcase .../> — add directly to suite
                    if let Some(ref mut suite) = current_suite {
                        suite.cases.push(tc);
                    }
                } else {
                    current_case = Some(tc);
                }
            }
            b"failure" => {
                let msg = e.attributes().flatten()
                    .find(|a| a.key.as_ref() == b"message")
                    .map(|a| String::from_utf8_lossy(&a.value).into());
                if let Some(ref mut tc) = current_case {
                    tc.status = TestCaseStatus::Failed { message: msg, body: None };
                }
                in_failure = !is_empty;
                text_buf.clear();
            }
            b"error" => {
                let msg = e.attributes().flatten()
                    .find(|a| a.key.as_ref() == b"message")
                    .map(|a| String::from_utf8_lossy(&a.value).into());
                if let Some(ref mut tc) = current_case {
                    tc.status = TestCaseStatus::Error { message: msg, body: None };
                }
                in_error = !is_empty;
                text_buf.clear();
            }
            b"skipped" => {
                let msg = e.attributes().flatten()
                    .find(|a| a.key.as_ref() == b"message")
                    .map(|a| String::from_utf8_lossy(&a.value).into());
                if let Some(ref mut tc) = current_case {
                    tc.status = TestCaseStatus::Skipped { message: msg };
                }
            }
            _ => {}
        }
    }

    // Handle case where there's a single suite without <testsuites> wrapper
    if let Some(suite) = current_suite.take() {
        result.suites.push(suite);
    }

    if result.suites.is_empty() {
        None
    } else {
        Some(result)
    }
}

pub fn render_test_results(suites: &TestSuites) -> Markup {
    let total = suites.total_tests();
    let failed = suites.total_failures();
    let errors = suites.total_errors();
    let skipped = suites.total_skipped();

    html! {
        div class="test-summary" {
            div class=(if suites.all_passed() { "test-summary-bar test-summary-pass" } else { "test-summary-bar test-summary-fail" }) {
                @if suites.all_passed() {
                    span class="test-summary-icon" { "\u{2713}" }
                    " All " (total) " tests passed"
                } @else {
                    span class="test-summary-icon" { "\u{2717}" }
                    " " (failed + errors) " of " (total) " tests failed"
                }
                @if skipped > 0 {
                    span class="test-summary-skipped" { " (" (skipped) " skipped)" }
                }
            }
        }

        @for suite in &suites.suites {
            div class="test-suite" {
                div class="test-suite-header" {
                    span class="test-suite-name" { (&suite.name) }
                    span class="test-suite-stats" {
                        (suite.tests) " tests"
                        @if let Some(t) = &suite.time {
                            " \u{00b7} " (t) "s"
                        }
                    }
                }
                @for tc in &suite.cases {
                    @let (icon, row_class) = match &tc.status {
                        TestCaseStatus::Passed => ("\u{2713}", "test-case test-case-passed"),
                        TestCaseStatus::Failed { .. } => ("\u{2717}", "test-case test-case-failed"),
                        TestCaseStatus::Error { .. } => ("\u{2717}", "test-case test-case-failed"),
                        TestCaseStatus::Skipped { .. } => ("\u{2014}", "test-case test-case-skipped"),
                    };
                    div class=(row_class) {
                        div class="test-case-header" {
                            span class="test-case-icon" { (icon) }
                            span class="test-case-name" {
                                @if let Some(cn) = &tc.classname {
                                    span class="test-case-classname" { (cn) " \u{203a} " }
                                }
                                (&tc.name)
                            }
                            @if let Some(t) = &tc.time {
                                span class="test-case-time" { (t) "s" }
                            }
                        }
                        @match &tc.status {
                            TestCaseStatus::Failed { message, body } | TestCaseStatus::Error { message, body } => {
                                div class="test-case-detail" {
                                    @if let Some(msg) = message {
                                        div class="test-case-message" { (msg) }
                                    }
                                    @if let Some(b) = body {
                                        pre class="test-case-body" { (b) }
                                    }
                                }
                            }
                            TestCaseStatus::Skipped { message: Some(msg) } => {
                                div class="test-case-detail" {
                                    div class="test-case-message" { (msg) }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}
