#[derive(Clone, Copy, Debug)]
pub enum DurabilityPolicy {
    SyncOnEveryPut,
    SyncOnInterval,
    OsDecides,
}
