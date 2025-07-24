// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(test)]

use super::{parse, Node};

fn parse_no_errors(input: &str) {
    let block = parse(input);
    let errors: Vec<_> = block.errors().collect();
    assert!(
        errors.is_empty(),
        "parse failed!\n\tinput = {:?}\n\terrors = {:?}",
        input,
        errors
    );
}

#[test]
fn smoke() {
    // Empty
    parse_no_errors("");
    parse_no_errors(" \r\n\t");

    // Assignment
    parse_no_errors("a = 1");
    parse_no_errors("a += 1");
    parse_no_errors("a -= 1");
    parse_no_errors("a[1] = 1");
    parse_no_errors("a.b = 1");

    // Conditional
    parse_no_errors("if (true) {}");
    parse_no_errors("if (true) { a = 1 }");
    parse_no_errors("if (true) {} else {}");
    parse_no_errors("if (true) { a = 1 } else { a = 2 }");
    parse_no_errors("if (true) {} else if (true) {}");
    parse_no_errors("if (true) { a = 1 } else if (true) { a = 2 }");
    parse_no_errors("if (true) {} else if (true) {} else {}");
    parse_no_errors("if (true) { a = 1 } else if (true) { a = 2 } else { a = 3 }");

    // Call
    parse_no_errors("assert(true)");
    parse_no_errors("declare_args() {}");
    parse_no_errors("declare_args() { a = 1 }");
    parse_no_errors("template(\"foo\") {}");
    parse_no_errors("template(\"foo\") { bar(target_name) }");

    // Expressions
    parse_no_errors("a = 1");
    parse_no_errors("a = b[1]");
    parse_no_errors("a = b.c");
    parse_no_errors("a = 1 + 2 - 3");
    parse_no_errors("a = 1 == 2");
    parse_no_errors("a = 1 <= 2");
    parse_no_errors("a = true && false || !false");
    parse_no_errors(r#"a = "foo\"bar\\baz""#);
    parse_no_errors("a = b(c)");
    parse_no_errors("a = b.c");
    parse_no_errors("a = {}");
    parse_no_errors("a = { b = 1 }");
    parse_no_errors("a = (((1)))");
    parse_no_errors("a = []");
    parse_no_errors("a = [1]");
    parse_no_errors("a = [1, ]");
    parse_no_errors("a = [1, 2]");
    parse_no_errors("a = [1, 2, ]");

    // TODO: Add more tests.
}

#[test]
fn comments() {
    parse_no_errors("# comment");
    parse_no_errors("# comment\n  # comment\n");
    parse_no_errors("a = 1 # comment");
}

#[test]
fn error_recovery() {
    parse("a = 1 2 3");
    parse("a = \"foo\nb = 1");
    parse("declare_args() {}}");

    // TODO: Add more tests.
}

#[test]
fn missing_comma() {
    let block = parse("a = [1, 2 3]");
    let errors: Vec<_> = block
        .errors()
        .map(|e| (e.span().start(), e.span().end()))
        .collect();
    assert_eq!(errors, [(9, 9)]);
}

#[test]
fn open_string() {
    let block = parse(
        r#"
a = [
  "aaa",
  "bb
  "ccc",
]
"#,
    );
    let errors: Vec<_> = block.errors().map(|e| e.span().as_str()).collect();
    assert_eq!(errors, ["\"bb\n", ""]);
}
