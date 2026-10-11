# logos-bitcoin-wallet-backend

`bitcoin_wallet_backend`: the coordinator between the Bitcoin wallet surfaces (the app, the
CLI, any module that asks for a payment) and `bitcoin_wallet_core_module`. It keeps the
custodian role, a registry of wallets opened before, and every send from request to
settlement, and it relays the source settings of `bitcoin_node_module`. It holds no key
material and takes no password: the person opens a wallet in the keys app and approves a
send in the signer app. It calls neither `signer_manager_module` nor `keystore_module`; the
wallet core is the requester and keeps the receipts, and this module sees only handles.

## Roles

One role, the custodian set, defaults to `bitcoin_wallet_ui`. There is no approver role.

| Who | Methods |
|---|---|
| custodian | `configure`, `set_active_network`, `open_wallet`, `close_wallet`, `descriptors`, `set_sources`, `set_source`, `set_proxy`, `clear_suspect`, `rebroadcast`, `abandon_send` |
| any named module | `request_send`, `request_fee_bump`, and `cancel_send` for its own request |
| anyone | every read: `caller_identity`, `list_networks`, `list_wallets`, `job_status`, `wallet_status`, `balances`, `history`, `receive_address`, `fee_rates`, `address_valid`, `format_btc`, `parse_btc`, `sources`, `source_health`, `exposure`, `send_status`, `list_sends` |

`configure({ custodians })` replaces the set. It is total (`{}` leaves nobody), refuses
unknown keys, and is saved to `roles.json` in the instance's persistence directory. Only a
custodian may call it, and `logosctl` is the host anchor, never a custodian. To enrol the
headless CLI, write the file named by `caller_identity`'s `rolesFile`, then run
`logosctl module reload bitcoin_wallet_backend`:

```json
{"custodians": ["bitcoin_wallet_ui", "bitcoin_wallet_cli"]}
```

A refused call answers exactly `{"ok":false,"error":"not authorized"}`.

## Opening a wallet

`open_wallet({ network?, group?, family?, birthday?, label? })` runs on the active network.
A `group` reopens a wallet; a `family` (`bitcoin` for native segwit, `bitcoin_taproot`) lets
the person pick, create or restore one. It answers `{ ok, jobId }`. While the person decides,
`wallet_status` carries the core's `openHandle`, which the app passes to the
`keystore.accounts.open` intent. Once open, the wallet is recorded in `wallets.json` with its
group, family, network, label and birthday, and nothing secret.

From the moment an open starts until the wallet closes, the backend tells the node module
(`set_wallet_open`), which then refuses source changes on that network. It checks the node
module every 9 s and says so again after a restart. `set_sources`, `set_source` and
`set_proxy` are refused here too while a wallet is open. `clear_suspect` is not: the core
marks a source suspect while it syncs, so the mark is cleared in the node module and, with a
wallet open, in the core.

## Sends

1. Any named module calls `request_send({ recipients: [{ address, amountSat | amountBtc }],
   feeRate? | target?, sendMax? })`, or `request_fee_bump({ txid, feeRate })` to replace an
   unconfirmed send with RBF. It gets a `requestId`. One send is open per wallet.
2. `preparing`: the core builds an unsigned PSBT and locks its inputs, and the backend asks it
   to sign and broadcast.
3. `awaiting_approval`: `send_status` carries the core's `approvalHandle` (`smh_…`), which the
   app passes to the `evm.signing.approve` intent, and the summary the app shows: recipients,
   change, fee, total and warnings.
4. `committing`: approved; the core collects the signed PSBT, checks it is the transaction it
   built, and broadcasts it.
5. `sent`, or `failed`, `cancelled` or `unknown`. These four are final.

- An offer nobody acknowledges within 60 s expires in the keystore. The send fails with a
  reason that says so and its inputs are unlocked; the app must request it again. Once the
  signer shows it, the person has no timeout.
- If an input leaves the wallet's confirmed coins while the send waits, the backend withdraws
  it and fails it.
- `unknown` means the transaction may have left. `send_status` offers `rebroadcast`,
  `request_fee_bump` and `abandon_send`; never a new send from other coins while it could still
  confirm. A rebroadcast the network has seen moves it to `sent`. `abandon_send({ txid })`
  (custodian, `unknown` only) has the core free its coins and marks the send `abandoned`: if the
  original still confirms, a later send from those coins fails, or replaces it if it confirms first.
- A failed or cancelled send carries `errorCode`: the core's `rejected`, `expired`, `cancelled`,
  `insufficient_funds`, `no_route`, `busy`, `refused`, `no_wallet`, `signer_unavailable`, `invalid`
  or `failed`, or the backend's `coins_changed`, `core_lost` or `backend_restarted`. A core that
  sends no code is read by its error text.
- Sends are kept in `sends.json`, which holds no receipt, key or password. After a restart an
  open send is reconciled with the core: a preview still awaiting approval is followed, one built
  but never signed is signed, and one the core no longer has is settled from the wallet's history
  (`sent` once a block or a source vouches for it, `unknown` if it may have left, otherwise
  `failed` with `backend_restarted`).
- `cancel_send` (the requester or a custodian) withdraws the approval request and unlocks the
  inputs. It is refused once the transaction is going out, and `close_wallet` withdraws open
  sends first.

## Everything else

Reads pass through with display amounts (`…Btc`, eight decimals) and `syncPercent`, counted
from the wallet's birthday and absent while a height is unknown. A busy core answers empty
amounts, never zero. `format_btc` and `parse_btc` are exact; more than eight decimals is
refused. Long operations answer a backend job id (`job_status`); the core's receipts never
leave this module. Events: `wallet_state_changed`, `sync_progress`, `balance_changed`,
`send_status_changed`, `job_finished`, `source_alert`.

Networks: `mainnet`, `testnet4`, `signet`, and `regtest` when the persistence directory holds
`regtest.json`; the wallet core and the node module need the same file in theirs. Persists
`roles.json`, `settings.json` (the active network), `wallets.json` and `sends.json`.

## Contracts

The wallet core and the node module have no published repository yet, so their contracts are
committed under `contracts/` and named by `dependency_overrides` in `metadata.json`.
Regenerate them from each module's `glue.rs` with `logos-lidl-gen --from-rust` when either
changes.

```bash
cd rust-lib && cargo test --no-default-features
nix build .#lgx
```
