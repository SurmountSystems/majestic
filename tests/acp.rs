use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use majestic::acp::methods_listed_in_doc;
use majestic::rpc::LOCAL_FUNCTION_METHODS;

#[test]
fn acp_methods_match_documented_local_functions() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/local-functions.md");
    let doc =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let documented = methods_listed_in_doc(&doc);
    assert!(
        !documented.is_empty(),
        "docs/local-functions.md must list ACP methods"
    );
    let dispatched: BTreeSet<&str> = LOCAL_FUNCTION_METHODS.iter().copied().collect();
    for method in &documented {
        assert!(
            dispatched.contains(method.as_str()),
            "docs list ACP method {method} but it has no dispatch method"
        );
    }
    for method in LOCAL_FUNCTION_METHODS {
        assert!(
            documented.iter().any(|listed| listed == method),
            "ACP dispatches {method} but docs/local-functions.md has no row"
        );
    }
}
