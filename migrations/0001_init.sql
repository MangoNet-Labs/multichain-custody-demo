-- PostgreSQL 17 / SQLx: initial public schema for a fresh demo installation.
-- Fresh installations only. SQLx verifies the baseline checksum on restart.

-- 1. Accounts, assets and custody wallets
-- ---------------------------------------

CREATE DOMAIN demo_network AS TEXT CHECK (VALUE IN ('mango-testnet', 'bsc-testnet'));
COMMENT ON DOMAIN demo_network IS 'The two supported testnets. The same symbol on different networks has separate accounting.';

CREATE DOMAIN digest32 AS BYTEA CHECK (octet_length(VALUE) = 32);
COMMENT ON DOMAIN digest32 IS 'Raw 32-byte digest or Mango object ID; never store its display encoding here.';

CREATE DOMAIN nonnegative_raw AS NUMERIC CHECK (
    VALUE >= 0 AND scale(VALUE) = 0 AND
    VALUE <= 115792089237316195423570985008687907853269984665640564039457584007913129639935
);
CREATE DOMAIN positive_raw AS nonnegative_raw CHECK (VALUE > 0);
COMMENT ON DOMAIN positive_raw IS 'Positive integer amount in the smallest asset unit, up to uint256.';

CREATE TABLE demo_users (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL
);
COMMENT ON TABLE demo_users IS 'Local demo accounts. Switching users in the UI is not authentication.';

CREATE TABLE assets (
    network demo_network NOT NULL,
    asset BYTEA NOT NULL,
    symbol TEXT NOT NULL,
    decimals SMALLINT NOT NULL CHECK (decimals BETWEEN 0 AND 255),
    enabled_from BIGINT NOT NULL DEFAULT 0 CHECK (enabled_from >= 0),
    opening_digest digest32,
    PRIMARY KEY (network, asset),
    CHECK (octet_length(asset) = 0 OR (network = 'bsc-testnet' AND octet_length(asset) = 20)
        OR (network = 'mango-testnet' AND octet_length(asset) BETWEEN 1 AND 512))
);
COMMENT ON TABLE assets IS 'Deposit allowlist, identified by network and contract, not ticker.';
COMMENT ON COLUMN assets.asset IS 'Empty for native coins; raw BSC contract address or UTF-8 canonical Mango coin type for tokens.';
COMMENT ON COLUMN assets.decimals IS 'Display precision. Persist and compare balances in raw integer units.';
COMMENT ON COLUMN assets.enabled_from IS 'First height at which this asset is eligible for scanning.';

COMMENT ON COLUMN assets.opening_digest IS 'Cursor digest immediately before a later listing; NULL for assets enabled at initialization.';

CREATE TABLE asset_registries (
    network demo_network PRIMARY KEY,
    initialized BOOLEAN NOT NULL DEFAULT false
);
COMMENT ON TABLE asset_registries IS 'Whether this network has activated its initial asset list under the cursor lock.';

CREATE TABLE custody_installation (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    id UUID NOT NULL DEFAULT gen_random_uuid()
);
COMMENT ON TABLE custody_installation IS 'One random installation identity binds the database to the encrypted wallet store.';

CREATE TABLE wallet_allocations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    network demo_network NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('deposit','hot','gas')),
    owner_id TEXT REFERENCES demo_users(id),
    slot TEXT GENERATED ALWAYS AS (CASE WHEN role='deposit' THEN 'deposit:' || owner_id ELSE role END) STORED,
    CHECK ((role='deposit') = (owner_id IS NOT NULL)),
    UNIQUE (network,slot)
);
COMMENT ON TABLE wallet_allocations IS 'Stable wallet generation requests; retries reuse the same slot and key ID.';
COMMENT ON COLUMN wallet_allocations.slot IS 'One deposit slot per user and network, or a shared platform hot/gas slot.';

CREATE TABLE wallets (
    network demo_network NOT NULL,
    address BYTEA NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('deposit','hot','gas')),
    owner_id TEXT REFERENCES demo_users(id),
    key_ref TEXT NOT NULL UNIQUE,
    PRIMARY KEY (network,address),
    CHECK ((role = 'deposit') = (owner_id IS NOT NULL)),
    CHECK (octet_length(address) = CASE WHEN network = 'mango-testnet' THEN 32 ELSE 20 END)
);
COMMENT ON TABLE wallets IS 'Registered addresses controlled by this installation. Only deposit wallets belong to a demo user.';
COMMENT ON COLUMN wallets.key_ref IS 'Handle for the encrypted key in the signer keystore; never a private key.';

CREATE UNIQUE INDEX user_deposit_wallet ON wallets(network,owner_id) WHERE role = 'deposit';
CREATE UNIQUE INDEX platform_wallet ON wallets(network,role) WHERE role <> 'deposit';

-- Track signed changes only for registered addresses. The token need not be
-- listed yet, so there is intentionally no foreign key to assets.
CREATE TABLE opening_balances (
    network demo_network NOT NULL, asset BYTEA NOT NULL CHECK (octet_length(asset)>0),
    address BYTEA NOT NULL, amount NUMERIC NOT NULL CHECK (scale(amount)=0),
    PRIMARY KEY(network,asset,address),
    FOREIGN KEY(network,address) REFERENCES wallets(network,address)
);
COMMENT ON TABLE opening_balances IS 'Pre-listing holdings; frozen at activation and never credited to a user.';

-- 2. Confirmed chain data and scanner health
-- ------------------------------------------

CREATE TABLE scan_cursors (
    network demo_network PRIMARY KEY,
    height BIGINT NOT NULL CHECK (height >= -1),
    digest digest32 NOT NULL,
    rules digest32,
    last_advanced_at TIMESTAMPTZ
);
COMMENT ON TABLE scan_cursors IS 'Authoritative last committed height and digest for each network.';
COMMENT ON COLUMN scan_cursors.rules IS 'Fingerprint of the scan rules used to interpret the committed chain data.';
COMMENT ON COLUMN scan_cursors.last_advanced_at IS 'Last cursor advance; diagnostic timestamp, not evidence of finality.';

CREATE TABLE scanned_blocks (
    network demo_network NOT NULL REFERENCES scan_cursors(network),
    height BIGINT NOT NULL CHECK (height >= 0),
    digest digest32 NOT NULL,
    parent digest32 NOT NULL,
    evidence_hash digest32 NOT NULL,
    mango_execution_hash digest32,
    opening_evidence digest32 NOT NULL,
    PRIMARY KEY (network,height),
    UNIQUE (network,digest)
);
COMMENT ON TABLE scanned_blocks IS 'Committed BSC blocks or Mango checkpoints anchoring payment and deposit evidence.';
COMMENT ON COLUMN scanned_blocks.mango_execution_hash IS 'Mango execution evidence used for replay validation; absent on BSC.';

CREATE TABLE scan_head_samples (
    network demo_network PRIMARY KEY REFERENCES scan_cursors(network),
    head BIGINT NOT NULL CHECK(head >= 0),
    committed BIGINT NOT NULL CHECK(committed BETWEEN -1 AND head),
    sampled_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
COMMENT ON TABLE scan_head_samples IS 'Last observed head and committed height for reporting lag. Never use as the accounting cursor.';

CREATE TABLE scan_issues (
    network demo_network PRIMARY KEY,
    stage TEXT NOT NULL CHECK (stage IN ('finality','range')),
    next_height BIGINT,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK ((stage = 'finality') = (next_height IS NULL)),
    CHECK (next_height >= 0)
);
COMMENT ON TABLE scan_issues IS 'Latest scanner failure by network, including a head lookup failure before the cursor exists.';

-- 3. Ledger and incoming transfers
-- --------------------------------

CREATE TABLE journal (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    network demo_network NOT NULL,
    asset BYTEA NOT NULL,
    debit_account TEXT NOT NULL,
    credit_account TEXT NOT NULL,
    amount positive_raw NOT NULL,
    CHECK (debit_account <> credit_account),
    FOREIGN KEY (network,asset) REFERENCES assets(network,asset)
);
COMMENT ON TABLE journal IS 'One row posts equal and opposite amounts between two accounts of the same network and asset.';

CREATE VIEW journal_entries AS
    SELECT id, network, asset, debit_account AS account, -amount AS delta FROM journal
    UNION ALL
    SELECT id, network, asset, credit_account AS account, amount AS delta FROM journal;
COMMENT ON VIEW journal_entries IS 'Signed debit and credit legs of each journal movement; sum by account for its balance.';

CREATE TABLE deposits (
    network demo_network NOT NULL,
    height BIGINT NOT NULL,
    tx digest32 NOT NULL,
    recipient BYTEA NOT NULL,
    sender BYTEA,
    asset BYTEA NOT NULL,
    log_index BIGINT,
    tx_log_index BIGINT,
    journal_id BIGINT PRIMARY KEY REFERENCES journal(id),
    CHECK ((log_index IS NULL) = (tx_log_index IS NULL)),
    CHECK ((network = 'bsc-testnet' AND octet_length(asset) <> 0) = (log_index IS NOT NULL)),
    CHECK (log_index >= 0 AND tx_log_index >= 0),
    FOREIGN KEY (network,height) REFERENCES scanned_blocks(network,height),
    FOREIGN KEY (network,recipient) REFERENCES wallets(network,address),
    FOREIGN KEY (network,asset) REFERENCES assets(network,asset)
);
COMMENT ON TABLE deposits IS 'Confirmed external deposits credited to user accounts. Partial unique indexes prevent double credit on replay.';
COMMENT ON COLUMN deposits.log_index IS 'Block-wide EVM token log index; NULL for native transfers.';
COMMENT ON COLUMN deposits.tx_log_index IS 'Transaction-local EVM token log index, independent of its position in the block.';

CREATE UNIQUE INDEX mango_deposit ON deposits(network,tx,recipient,asset) WHERE network = 'mango-testnet';
CREATE UNIQUE INDEX evm_native_deposit ON deposits(network,tx) WHERE network = 'bsc-testnet' AND log_index IS NULL;
CREATE UNIQUE INDEX evm_token_log ON deposits(network,height,log_index) WHERE log_index IS NOT NULL;
CREATE UNIQUE INDEX evm_tx_log ON deposits(network,tx,tx_log_index) WHERE log_index IS NOT NULL;

CREATE TABLE internal_receipts (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    network demo_network NOT NULL,
    height BIGINT NOT NULL,
    tx digest32 NOT NULL,
    sender BYTEA NOT NULL,
    recipient BYTEA NOT NULL,
    asset BYTEA NOT NULL,
    amount positive_raw NOT NULL,
    log_index BIGINT,
    FOREIGN KEY (network,height) REFERENCES scanned_blocks(network,height),
    FOREIGN KEY (network,sender) REFERENCES wallets(network,address),
    FOREIGN KEY (network,recipient) REFERENCES wallets(network,address),
    FOREIGN KEY (network,asset) REFERENCES assets(network,asset)
);
COMMENT ON TABLE internal_receipts IS 'Transfers between managed addresses. A sweep or BNB gas top-up is not a second user deposit.';

CREATE TABLE mango_internal_outputs (
    network demo_network NOT NULL DEFAULT 'mango-testnet' CHECK (network = 'mango-testnet'),
    tx digest32 NOT NULL,
    sender BYTEA NOT NULL,
    recipient BYTEA NOT NULL,
    amount nonnegative_raw NOT NULL,
    order_ref TEXT NOT NULL,
    asset BYTEA NOT NULL DEFAULT '',
    PRIMARY KEY (network,tx,recipient,asset),
    FOREIGN KEY (network,asset) REFERENCES assets(network,asset),
    FOREIGN KEY (network,sender) REFERENCES wallets(network,address),
    FOREIGN KEY (network,recipient) REFERENCES wallets(network,address)
);
COMMENT ON TABLE mango_internal_outputs IS 'Expected Mango internal receipts recorded before broadcast; a balance delta alone cannot identify its payer.';

CREATE TABLE platform_funding (
    network demo_network NOT NULL, height BIGINT NOT NULL, tx digest32 NOT NULL,
    recipient BYTEA NOT NULL, asset BYTEA NOT NULL, log_index BIGINT, sender BYTEA,
    journal_id BIGINT PRIMARY KEY REFERENCES journal(id),
    UNIQUE NULLS NOT DISTINCT(network,tx,recipient,asset,log_index),
    FOREIGN KEY(network,height) REFERENCES scanned_blocks(network,height),
    FOREIGN KEY(network,recipient) REFERENCES wallets(network,address),
    FOREIGN KEY(network,asset) REFERENCES assets(network,asset)
);
COMMENT ON TABLE platform_funding IS 'External funding of hot or gas wallets. Credits platform capital rather than a user balance.';

-- 4. Withdrawal and sweep orders
-- ------------------------------

CREATE TABLE payment_control (
    singleton BOOLEAN PRIMARY KEY DEFAULT true CHECK(singleton),
    paused BOOLEAN NOT NULL DEFAULT false
);
COMMENT ON TABLE payment_control IS 'Singleton switch for preparing payments; does not erase already signed transactions.';

CREATE TABLE withdrawals (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id TEXT NOT NULL REFERENCES demo_users(id),
    request_id TEXT NOT NULL CHECK (length(request_id) BETWEEN 1 AND 128),
    network demo_network NOT NULL,
    asset BYTEA NOT NULL,
    recipient BYTEA NOT NULL,
    amount positive_raw NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN
        ('queued','consolidating','signed','submitted','completed','failed','review')),
    CHECK (status<>'consolidating' OR network='mango-testnet'),
    freeze_id BIGINT NOT NULL UNIQUE REFERENCES journal(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    detail TEXT,
    next_prepare TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (network,asset) REFERENCES assets(network,asset),
    CHECK (octet_length(recipient) = CASE WHEN network='bsc-testnet' THEN 20 ELSE 32 END),
    UNIQUE (user_id,request_id)
);
COMMENT ON TABLE withdrawals IS 'User withdrawal requests. A successful broadcast is not final settlement.';
COMMENT ON COLUMN withdrawals.request_id IS 'Client idempotency key, scoped to the user.';
COMMENT ON COLUMN withdrawals.freeze_id IS 'Journal movement from the user available balance to the frozen balance.';

CREATE INDEX withdrawals_work ON withdrawals(network,created_at,id) WHERE status='queued';

CREATE TABLE sweep_rules (
    network demo_network NOT NULL, asset BYTEA NOT NULL, minimum positive_raw NOT NULL,
    PRIMARY KEY(network,asset), FOREIGN KEY(network,asset) REFERENCES assets(network,asset)
);
COMMENT ON TABLE sweep_rules IS 'Minimum confirmed balance required to start a sweep for each asset.';
COMMENT ON COLUMN sweep_rules.minimum IS 'Compare against gross balance before reserving the native transaction fee.';

CREATE TABLE sweep_checks (
    network demo_network NOT NULL, address BYTEA NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1, next_check TIMESTAMPTZ DEFAULT now(),
    finalized_failures SMALLINT NOT NULL DEFAULT 0 CHECK (finalized_failures BETWEEN 0 AND 4),
    lease UUID, lease_until TIMESTAMPTZ, detail TEXT NOT NULL DEFAULT 'checking confirmed balance',
    PRIMARY KEY(network,address), FOREIGN KEY(network,address) REFERENCES wallets(network,address),
    CHECK((lease IS NULL)=(lease_until IS NULL))
);
COMMENT ON TABLE sweep_checks IS 'Durable queue for rechecking deposit wallets when confirmed funds change.';
COMMENT ON COLUMN sweep_checks.revision IS 'Generation counter prevents a concurrent update from being lost when a check finishes.';
COMMENT ON COLUMN sweep_checks.next_check IS 'NULL when the address does not currently need another check.';

CREATE INDEX sweep_checks_due ON sweep_checks(network,next_check) WHERE next_check IS NOT NULL;

CREATE TABLE sweep_orders (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(), network demo_network NOT NULL,
    deposit_address BYTEA NOT NULL, sender BYTEA NOT NULL, recipient BYTEA NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('sweep','gas_topup')),
    trigger_asset BYTEA NOT NULL, asset BYTEA NOT NULL, amount nonnegative_raw NOT NULL,
    minimum positive_raw NOT NULL, observed_balance positive_raw NOT NULL,
    observed_height BIGINT NOT NULL CHECK(observed_height>=0), observed_hash digest32 NOT NULL,
    status TEXT NOT NULL DEFAULT 'signed' CHECK(status IN ('signed','submitted','completed','failed','review')),
    detail TEXT,created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY(network,deposit_address) REFERENCES wallets(network,address),
    FOREIGN KEY(network,sender) REFERENCES wallets(network,address),
    FOREIGN KEY(network,recipient) REFERENCES wallets(network,address),
    FOREIGN KEY(network,asset) REFERENCES assets(network,asset),
    FOREIGN KEY(network,trigger_asset) REFERENCES assets(network,asset),
    CHECK(sender<>recipient AND observed_balance>=minimum),
    CHECK(amount>0 OR (network='mango-testnet' AND kind='sweep')),
    CHECK((kind='sweep' AND deposit_address=sender AND asset=trigger_asset)
       OR (kind='gas_topup' AND deposit_address=recipient AND network='bsc-testnet' AND asset='' AND trigger_asset<>''))
);
COMMENT ON TABLE sweep_orders IS 'Moves custody funds internally: sweep a deposit to hot, or top up a BSC deposit address for token gas.';
COMMENT ON COLUMN sweep_orders.trigger_asset IS 'Asset that crossed the sweep threshold; may differ from BNB sent for a gas top-up.';
COMMENT ON COLUMN sweep_orders.observed_height IS 'Confirmed height at which the triggering balance was observed.';

CREATE UNIQUE INDEX sweep_one_deposit ON sweep_orders(network,deposit_address) WHERE status IN ('signed','submitted','review');
CREATE UNIQUE INDEX sweep_one_sender ON sweep_orders(network,sender) WHERE status IN ('signed','submitted','review');

-- 5. Signed transactions and reserved inputs
-- ------------------------------------------

CREATE TABLE evm_payouts (
    withdrawal_id UUID UNIQUE REFERENCES withdrawals(id),
    network demo_network NOT NULL DEFAULT 'bsc-testnet' CHECK(network='bsc-testnet'),
    sender BYTEA NOT NULL,
    nonce BIGINT NOT NULL CHECK(nonce>=0),
    digest digest32 PRIMARY KEY,
    signed_tx BYTEA NOT NULL CHECK(octet_length(signed_tx)>0),
    max_fee positive_raw NOT NULL,
    next_send TIMESTAMPTZ NOT NULL DEFAULT now(),
    send_count INT NOT NULL DEFAULT 0,
    confirmed_height BIGINT,
    observed_tx digest32,
    settlement_journal BIGINT UNIQUE REFERENCES journal(id),
    gas_journal BIGINT UNIQUE REFERENCES journal(id),
    sweep_id UUID UNIQUE REFERENCES sweep_orders(id),
    CHECK (num_nonnulls(withdrawal_id, sweep_id) = 1),
    UNIQUE(network,sender,nonce),
    FOREIGN KEY(network,sender) REFERENCES wallets(network,address),
    FOREIGN KEY(network,confirmed_height) REFERENCES scanned_blocks(network,height)
);
COMMENT ON TABLE evm_payouts IS 'Durable BSC signed transactions shared by withdrawals and sweeps; only one can reserve a sender nonce.';
COMMENT ON COLUMN evm_payouts.signed_tx IS 'Exact signed bytes to rebroadcast when the original RPC acknowledgement is lost.';
COMMENT ON COLUMN evm_payouts.confirmed_height IS 'Block committed by the scanner; not set merely because a node accepts the broadcast.';

CREATE INDEX evm_payouts_pending ON evm_payouts(next_send) WHERE confirmed_height IS NULL;

CREATE TABLE mango_wallet_coins (
    object_id digest32 PRIMARY KEY,
    network demo_network NOT NULL DEFAULT 'mango-testnet' CHECK(network='mango-testnet'),
    owner BYTEA NOT NULL, version BIGINT NOT NULL CHECK(version>=0),
    digest digest32 NOT NULL, balance NUMERIC(20,0) NOT NULL CHECK(balance BETWEEN 0 AND 18446744073709551615),
    changed_height BIGINT NOT NULL,
    asset BYTEA NOT NULL DEFAULT '',
    FOREIGN KEY(network,asset) REFERENCES assets(network,asset),
    FOREIGN KEY(network,owner) REFERENCES wallets(network,address),
    FOREIGN KEY(network,changed_height) REFERENCES scanned_blocks(network,height)
);
COMMENT ON TABLE mango_wallet_coins IS 'Finalized allowlisted coin objects owned by managed wallets, identified by coin type.';

CREATE INDEX mango_coins_by_owner ON mango_wallet_coins(owner,asset,balance DESC,object_id);

CREATE TABLE mango_payouts (
    withdrawal_id UUID UNIQUE REFERENCES withdrawals(id),
    merge_for UUID REFERENCES withdrawals(id),
    network demo_network NOT NULL DEFAULT 'mango-testnet' CHECK(network='mango-testnet'),
    sender BYTEA NOT NULL,
    digest digest32 NOT NULL UNIQUE,
    tx_bytes BYTEA NOT NULL CHECK(octet_length(tx_bytes)>0),
    signature BYTEA NOT NULL CHECK(octet_length(signature)=97 AND get_byte(signature,0)=0),
    budget positive_raw NOT NULL,
    next_send TIMESTAMPTZ NOT NULL DEFAULT now(),
    send_count INTEGER NOT NULL DEFAULT 0,
    confirmed_height BIGINT,
    observed_tx digest32,
    net_gas BIGINT,
    settlement_journal BIGINT UNIQUE REFERENCES journal(id),
    gas_journal BIGINT UNIQUE REFERENCES journal(id),
    sweep_id UUID UNIQUE REFERENCES sweep_orders(id),
    sponsor BYTEA,
    sponsor_signature BYTEA,
    payout_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    CONSTRAINT mango_sponsor_role_fk FOREIGN KEY(network,sponsor) REFERENCES wallets(network,address),
    CHECK (num_nonnulls(withdrawal_id,sweep_id,merge_for)=1),
    CHECK ((sweep_id IS NOT NULL)=(sponsor IS NOT NULL) AND (sponsor IS NULL)=(sponsor_signature IS NULL)),
    CHECK (sponsor<>sender AND octet_length(sponsor_signature)=97 AND get_byte(sponsor_signature,0)=0),
    FOREIGN KEY(network,sender) REFERENCES wallets(network,address),
    FOREIGN KEY(network,confirmed_height) REFERENCES scanned_blocks(network,height)
);
COMMENT ON TABLE mango_payouts IS 'Durable Mango signed transactions shared by withdrawals and sponsored sweeps.';
COMMENT ON COLUMN mango_payouts.sponsor IS 'Gas owner for sponsored sweeps; the sender pays gas for a normal withdrawal.';
COMMENT ON COLUMN mango_payouts.net_gas IS 'Net Mango fee in raw units after the storage rebate; may be negative.';
COMMENT ON COLUMN mango_payouts.merge_for IS 'Internal hot-wallet coin consolidation for this withdrawal; never posts principal.';

CREATE INDEX mango_payout_sendable ON mango_payouts(next_send) WHERE confirmed_height IS NULL;
CREATE UNIQUE INDEX mango_hot_one_inflight ON mango_payouts(sender) WHERE sponsor IS NULL AND confirmed_height IS NULL;
CREATE INDEX mango_merge_history ON mango_payouts(merge_for) WHERE merge_for IS NOT NULL;

CREATE TABLE mango_payout_inputs (
    object_id digest32 NOT NULL,
    version BIGINT NOT NULL CHECK (version >= 0),
    object_digest digest32 NOT NULL,
    payout_id UUID NOT NULL REFERENCES mango_payouts(payout_id),
    reserved BOOLEAN NOT NULL DEFAULT true,
    is_gas BOOLEAN NOT NULL DEFAULT true,
    input_index SMALLINT CHECK (input_index >= 0),
    PRIMARY KEY (payout_id,object_id,version)
);
COMMENT ON TABLE mango_payout_inputs IS 'Coin object versions bound to a signed Mango transaction. An active reservation prevents reuse.';
COMMENT ON COLUMN mango_payout_inputs.is_gas IS 'Distinguishes gas payment coins from ordinary transaction inputs.';
COMMENT ON COLUMN mango_payout_inputs.input_index IS 'Position for ordered inputs when the transaction records an ordering.';

CREATE UNIQUE INDEX mango_reserved_version ON mango_payout_inputs(object_id,version) WHERE reserved;
CREATE UNIQUE INDEX mango_ordered_inputs ON mango_payout_inputs(payout_id,input_index) WHERE input_index IS NOT NULL;

-- 6. Reconciliation at a confirmed height
-- ---------------------------------------

CREATE VIEW custody_wallet_movements AS
SELECT d.network,d.height,d.recipient AS address,d.asset,j.amount AS delta FROM deposits d JOIN journal j ON j.id=d.journal_id
UNION ALL SELECT f.network,f.height,f.recipient,f.asset,j.amount FROM platform_funding f JOIN journal j ON j.id=f.journal_id
UNION ALL SELECT network,height,recipient,asset,amount FROM internal_receipts
UNION ALL SELECT network,height,sender,asset,-amount FROM internal_receipts
UNION ALL SELECT w.network,coalesce(e.confirmed_height,m.confirmed_height),coalesce(e.sender,m.sender),w.asset,-w.amount
  FROM withdrawals w LEFT JOIN evm_payouts e ON e.withdrawal_id=w.id LEFT JOIN mango_payouts m ON m.withdrawal_id=w.id
  WHERE w.status='completed'
UNION ALL SELECT e.network,e.confirmed_height,e.sender,''::bytea,-j.amount FROM evm_payouts e JOIN journal j ON j.id=e.gas_journal
UNION ALL SELECT m.network,m.confirmed_height,coalesce(m.sponsor,m.sender),''::bytea,-m.net_gas::numeric FROM mango_payouts m WHERE m.net_gas IS NOT NULL;
COMMENT ON VIEW custody_wallet_movements IS 'Per-wallet signed movements. Internal transfers net to zero across custody; charge transaction fees to the actual gas payer.';

CREATE TABLE reconciliation_runs (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    network demo_network NOT NULL, height BIGINT NOT NULL, digest digest32 NOT NULL,
    checked_at TIMESTAMPTZ NOT NULL DEFAULT now(), matched BOOLEAN NOT NULL DEFAULT false,
    FOREIGN KEY(network,height) REFERENCES scanned_blocks(network,height)
);
COMMENT ON TABLE reconciliation_runs IS 'One comparison of ledger and chain state at a specific committed block or checkpoint.';

CREATE INDEX reconciliation_latest ON reconciliation_runs(network,height DESC,id DESC);

CREATE TABLE reconciliation_wallets (
    run_id BIGINT NOT NULL REFERENCES reconciliation_runs, address BYTEA NOT NULL, asset BYTEA NOT NULL,
    expected NUMERIC NOT NULL, observed NUMERIC NOT NULL CHECK(observed>=0 AND scale(observed)=0),
    PRIMARY KEY(run_id,address,asset)
);
COMMENT ON TABLE reconciliation_wallets IS 'Expected book balance and observed chain balance per managed wallet and asset.';

CREATE TABLE reconciliation_assets (
    run_id BIGINT NOT NULL REFERENCES reconciliation_runs, asset BYTEA NOT NULL,
    available NUMERIC NOT NULL, frozen NUMERIC NOT NULL, capital NUMERIC NOT NULL, net_fees NUMERIC NOT NULL,
    book_assets NUMERIC NOT NULL, observed NUMERIC NOT NULL,
    opening NUMERIC NOT NULL,
    PRIMARY KEY(run_id,asset)
);
COMMENT ON TABLE reconciliation_assets IS 'Balances by asset for a reconciliation run: liabilities, capital, fees, and observed custody funds.';

CREATE TABLE reconciliation_holds (
    network demo_network PRIMARY KEY, run_id BIGINT NOT NULL REFERENCES reconciliation_runs
);
COMMENT ON TABLE reconciliation_holds IS 'Evidence of an unresolved mismatch on this network. Payments cannot be resumed while any hold exists.';

CREATE TABLE reconciliation_mango_coins (
    run_id BIGINT NOT NULL REFERENCES reconciliation_runs, object_id digest32 NOT NULL,
    owner BYTEA NOT NULL CHECK(octet_length(owner)=32), balance NUMERIC(20,0) NOT NULL CHECK(balance>=0),
    asset BYTEA NOT NULL DEFAULT '',
    PRIMARY KEY(run_id,object_id)
);
COMMENT ON TABLE reconciliation_mango_coins IS 'Mango coin inventory captured for the reconciliation height.';

-- 7. Fresh-install data (no keys, addresses or funds)
-- ---------------------------------------------------

INSERT INTO demo_users (id, display_name) VALUES
    ('alice', 'Alice'),
    ('bob', 'Bob');

INSERT INTO assets(network,asset,symbol,decimals) VALUES
    ('mango-testnet', '', 'MGO', 9),
    ('bsc-testnet', '', 'BNB', 18);

INSERT INTO asset_registries(network) VALUES ('bsc-testnet'), ('mango-testnet');

INSERT INTO custody_installation DEFAULT VALUES;

INSERT INTO payment_control DEFAULT VALUES;
