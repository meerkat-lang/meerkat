use super::helpers::check_program;

#[test]
fn test_dependency_analysis_2() {
    let code = r#"
        service s1 {
            def store = fn (f: (int) -> int) => [f]; // push the function into a singleton list
            def d = store(fn (x: int) => x + a);
            var a = 0; // should be safe!
        }
    "#;
    let res = check_program(code);
    assert!(
        res.is_ok(),
        "Expected compilation to succeed, but got: {:?}",
        res
    );
}
