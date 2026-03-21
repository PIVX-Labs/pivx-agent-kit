use crate::mainnet_checkpoints::MAINNET_CHECKPOINTS;

/// Return the closest checkpoint at or before the given block height.
/// Returns (height, commitment_tree_hex).
pub fn get_checkpoint(block_height: i32) -> (i32, &'static str) {
    MAINNET_CHECKPOINTS
        .iter()
        .rev()
        .find(|cp| cp.0 <= block_height)
        .copied()
        .unwrap_or(MAINNET_CHECKPOINTS[0])
}
