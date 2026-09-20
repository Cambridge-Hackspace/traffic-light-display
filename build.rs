fn main() {
    // esp-idf-sys, which produces the sysenv, is only in the dependency graph
    // for the espidf target; on the host (unit tests, simulator) there is
    // nothing to forward.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("espidf") {
        embuild::espidf::sysenv::output();
    }
}
