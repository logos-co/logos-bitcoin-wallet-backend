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
| custodian | `configure`, `set_active_network`, `open_wallet`, `close_wallet`, `descriptors`, `set_birthday`, `set_gap_limit`, `set_sources`, `set_source`, `set_proxy`, `set_own_peers`, `peer_list`, `clear_suspect`, `rebroadcast`, `abandon_send` |
| any named module | `request_send`, `request_fee_bump`, and `cancel_send` for its own request |
| anyone | every read: `caller_identity`, `list_networks`, `list_wallets`, `job_status`, `wallet_status`, `balances`, `history`, `receive_address`, `current_address`, `fee_rates`, `address_valid`, `format_btc`, `parse_btc`, `sources`, `source_health`, `probe`, `exposure`, `send_status`, `list_sends` |

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

`open_wallet({ network?, group?, family?, birthday?, label?, gapLimit? })` runs on the active network.
A `group` reopens a wallet; a `family` (`bitcoin` for native segwit, `bitcoin_taproot`) lets
the person pick, create or restore one. It answers `{ ok, jobId }`. While the person decides,
`wallet_status` carries the core's `openHandle`, which the app passes to the
`keystore.accounts.open` intent. Once open, the wallet is recorded in `wallets.json` with its
group, family, network, label, birthday and gap limit, and nothing secret.

`set_birthday({ date: "YYYY-MM-DD" } | { height })` gives the open wallet a birthday later, for a
restore that had none: `{ ok, jobId }`, and the job's result is `{ birthday: { height, source },
rescanFrom }`. The core refuses a birthday after the wallet's first transaction, or a day still to
come, and its code stays on the job. The registry keeps the new birthday. While the rescan runs,
`wallet_status.rescanFrom` holds the height; `syncPercent` then counts Kyoto's share done from
there, and is absent under a history server's full scan, which reports no height.

The gap limit is how many unused addresses the core watches past the last used one, per
keychain: 20 to 1000. `open_wallet`'s `gapLimit` sets it; left out, the core keeps the wallet's
stored one, or 20. `set_gap_limit({ gapLimit })` changes it for the open wallet, as a wallet
setting rather than a source one, so an open wallet doesn't stop it: `{ ok, jobId }`, and the
job's result is `{ gapLimit, rescanFrom }`; a raise scans again from the birthday. A value out of
range is refused here with `code: "invalid"`, a closed wallet with `no_wallet`, and the core's
own codes (`invalid`, `busy`) pass through. `wallet_status.gapLimit` is the core's, and
`wallets.json` follows it. Coins found past the limit stay spendable but are listed apart:
`balances.beyondGap { count, sat, confirmedSat }` with `beyondGapBtc` and `beyondGapConfirmedBtc`
(all of them, and the confirmed part), left out of `confirmed`, `waiting`, `pending` and `sent`,
and `beyondGap` on every history row, true when the wallet's part is only through such
addresses. A send that spends them carries the core's `beyond_gap_inputs` warning in its summary.

From the moment an open starts until the wallet closes, the backend tells the node module
(`set_wallet_open`), which then refuses source changes on that network. It checks the node
module every 9 s and says so again after a restart. `set_sources`, `set_source` and
`set_proxy` are refused here too while a wallet is open. `clear_suspect` is not: the core
marks a source suspect while it syncs, so the mark is cleared in the node module and, with a
wallet open, in the core.

## Sends

1. Any named module calls `request_send({ recipients: [{ address, amountSat | amountBtc }],
   feeRate? | target?, sendMax?, smallChangeToFee? })`, or `request_fee_bump({ txid, feeRate })`
   to replace an unconfirmed send with RBF. It gets a `requestId`. One send is open per wallet.
   With `smallChangeToFee`, change too small to spend at the rate goes to the fee; the summary's
   `warnings` carry the core's `change_to_fee` or `change_too_small` as given.
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
  confirm. A rebroadcast the network has seen moves it to `sent`, and so does the core's history
  showing the transaction confirmed on the authority's chain, abandoned or not. `abandon_send({ txid })`
  (custodian, `unknown` only) has the core free its coins and marks the send `abandoned`: if the
  original still confirms, a later send from those coins fails, or replaces it if it confirms first.
- A failed or cancelled send carries `errorCode`: the core's `rejected`, `expired`, `cancelled`,
  `insufficient_funds`, `no_route`, `busy`, `refused`, `no_wallet`, `signer_unavailable`, `invalid`
  or `failed`, or the backend's `coins_changed`, `core_lost` or `backend_restarted`. A core that
  sends no code is read by its error text.
- Sends are kept in `sends.json`, which holds no receipt, key or password. After a restart an
  open send is reconciled with the core: a preview still awaiting approval is followed, one built
  but never signed is signed, and one the core no longer has is settled from the wallet's history.
  The core's own send record comes first, as its answer would have: a row's `broadcast` `seen` or
  `requested` is `sent`, with `broadcast` and `via` filled in, at once (Kyoto reports no mempool to
  wait for); `unknown` stays `unknown`, with its offer. A core whose rows name no outcome is
  believed on `sendState` `sent` or `replaced`. Then a block or a source vouching for it is `sent`;
  a core without send records waits 30 s for a source, then `unknown`; one seen broadcasting that
  the history lacks is `unknown`; otherwise `failed` with `backend_restarted`.
- An `unknown` send, or one a restart settled, moves to `sent` once the history shows it confirmed
  on the authority's chain. The record or a source does that too, unless the core said `unknown`
  or the send was abandoned.
- `cancel_send` (the requester or a custodian) withdraws the approval request and unlocks the
  inputs. It is refused once the transaction is going out, and `close_wallet` withdraws open
  sends first.

## Switching networks

`set_active_network` refuses while a wallet is open (`code: "wallet_open"`), a send is open
(`send_open`) or a wallet is opening (`busy`). While a close settles it queues the switch:
`{ ok, active, pending }`, also shown by `list_networks`. The switch applies once the close has
settled, and until then `open_wallet` answers `code: "busy"`, so an open never uses the old
network.

## Sources

`sources`, `source_health` and `probe({ network?, source })` pass the node module's rows through:
`certSha256`, `certificate { certSha256, subject, issuer, notBefore, notAfter, trusted }`, `encrypted`
and `userAdded`. `set_source` and `set_sources` take `certSha256` to pin a TLS server's certificate,
and null or `""` clears it; like every source change they need the custodian and a closed wallet.
On regtest the wallet core takes its sources from its own `regtest.json`, so pins there are seen
by the node module's probes only. `set_own_peers({ network, peers })` sets Kyoto's own peers, at
most 64 (`[]` returns to the curated list), and `peer_list({ network? })` reads them with the
list; both are custodian only, and the change is refused while a wallet is open.

When the wallet core or the node module stops answering, the reactor asks again after 100 ms,
doubling to 5 s, and stops once unload begins.

## Everything else

Reads pass through with display amounts (`…Btc`, eight decimals) and `syncPercent`, counted
from the wallet's birthday and absent while a height is unknown or the chain's headers are still
coming in. Everything else the core says passes as it is: `lastReorg`, `rescanFrom`,
`birthdaySource`, `node`, `kyoto` and the authority's `kind`, `base`, `asked` (false until the
open's first round of asking the chain's sources ends, or a server's first headers are taken) and
`headers` (`{ have, target }` during a header download, `target` null for Kyoto; null otherwise) in
`wallet_status`, and `headers` in `sync_progress` too; `blockHash`, `sendState`, `confirmedVia` and the `unchecked`
state on history rows; `authority_suspect` and `unchecked_anchor` alerts. `balances` gives
`unchecked` its `uncheckedBtc` twin like every other amount, and `beyondGap` its `beyondGapBtc`. A busy core answers empty
amounts, never zero. `receive_address` is the core's `next_address`: a fresh address, or, once
`unused` unused addresses reach the gap limit, the first of them with `fresh` false.
`current_address` is the first unused one, revealed only when none is: what the app shows when a
wallet opens. `fee_rates` is an estimate (`estimated`, with the `sources` it came from), or `{ ok:
false, error: "no fee estimate yet" }`, never the floor in its place. `format_btc` and `parse_btc` are exact; more than eight decimals is
refused. Long operations answer a backend job id (`job_status`); the core's receipts never
leave this module. Events: `wallet_state_changed`, `sync_progress`, `balance_changed`,
`send_status_changed`, `job_finished`, `source_alert`.

Networks: `mainnet`, `testnet4`, `signet`, and `regtest` when the persistence directory holds
`regtest.json`; the wallet core and the node module need the same file in theirs. Persists
`roles.json`, `settings.json` (the active network), `wallets.json` and `sends.json`.

## Contracts

The wallet core's and the node module's contracts are committed under `contracts/` and named
by `dependency_overrides` in `metadata.json`, so the module builds without their sources.
Regenerate them from each module's `glue.rs` with `logos-lidl-gen --from-rust` when either
changes. The core's is generated from its glue at 7a13513.

```bash
cd rust-lib && cargo test --no-default-features
nix build .#lgx
```
