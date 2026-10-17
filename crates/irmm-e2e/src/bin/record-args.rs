//! Stand-in agent binary for installer tests: records its arguments to `$IRMM_TEST_RECORD`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::fs::write(std::env::var("IRMM_TEST_RECORD").unwrap(), args.join(" ")).unwrap();
}
