/// Initial publication authority for Version migration. Keep this separate from agent keys.
module agent_wallet::publisher;

public struct PUBLISHER has drop {}

fun init(otw: PUBLISHER, ctx: &mut TxContext) {
    sui::package::claim_and_keep(otw, ctx);
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(PUBLISHER {}, ctx);
}
