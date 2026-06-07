# Zingo CLI

A command-line interface for the Zingo wallet.

## Building

### Default Build (Mainnet/Testnet)

To build the standard zingo-cli binary that works with mainnet and testnet:

```bash
cargo build --release
```

The binary will be available at `target/release/zingo-cli`.

### Build with Regtest Support

To build zingo-cli with regtest support in addition to mainnet and testnet:

```bash
cargo build --release --features regtest
```

The binary will be available at `target/release/zingo-cli`.

## Running

By default, zingo-cli stores wallet data in a `wallets/` directory in the current working directory.

The `--chain` argument allows you to select which network to connect to. If not specified, it defaults to mainnet.

### Mainnet

To connect to mainnet (default):

```bash
# Uses default wallet location: ./wallets/
./target/release/zingo-cli

# Or explicitly specify mainnet:
./target/release/zingo-cli --chain mainnet

# Or specify a custom data directory:
./target/release/zingo-cli --data-dir /path/to/mainnet-wallet
```

### Testnet

To connect to testnet:

```bash
# Uses default wallet location: ./wallets/
./target/release/zingo-cli --chain testnet

# Or specify a custom data directory:
./target/release/zingo-cli --chain testnet --data-dir /path/to/testnet-wallet
```

### Regtest Mode

To run in regtest mode:
1. Build the zingo-cli binary with the `regtest` feature flag enabled
```bash
cargo build --release -p zingo-cli --features regtest
```
2. Launch a validator, see details below for an example of launching zcashd and generating blocks with zcash-cli.
3. Launch an indexer/lightserver, see details below for an example of launching lightwalletd.
4. Create a wallet directory (data-dir) and run zingo-cli,
```bash
./target/release/zingo-cli --chain regtest --server 127.0.0.1:9067 --data-dir ~/tmp/regtest_temp
```

**Note:** The zcash_local_net crate will soon offer a binary for simplifying the process of launching and interacting with the local network.
https://github.com/zingolabs/infrastructure/tree/dev/zcash_local_net

#### Example: Launching a Local Network

1. Create a directory for zcashd with a `data` directory inside.
2. Add a `zcash.conf` config file to the main zcashd directory, see below for an example config.
3. Run zcashd:
```bash
zcashd --printtoconsole --conf=/home/user/tmp/zcashd_regtest/zcash.conf --datadir=/home/user/tmp/zcashd_regtest/data -debug=1
```
4. Create a directory for lightwalletd with a `data` and `logs` directory inside.
5. Create a `lwd.log` file inside the `logs` directory.
6. Add a `lightwalletd.yml` config file to the main lightwalletd directory, see below for an example config.
7. In a new command prompt, run lightwalletd:
```bash
lightwalletd --no-tls-very-insecure --data-dir /home/user/tmp/lwd_regtest/data/ --log-file /home/user/tmp/lwd_regtest/logs/lwd.log --zcash-conf-path /home/user/tmp/zcashd_regtest/zcash.conf --config /home/user/tmp/lwd_regtest/lightwalletd.yml
```
8. In a new command prompt, generate blocks:
```bash
zcash-cli -conf=/home/user/tmp/zcashd_regtest/zcash.conf generate 1
```

### Example: Zcashd Config File

```
### Blockchain Configuration
regtest=1
nuparams=5ba81b19:1 # Overwinter
nuparams=76b809bb:1 # Sapling
nuparams=2bb40e60:1 # Blossom
nuparams=f5b9230b:1 # Heartwood
nuparams=e9ff75a6:1 # Canopy
nuparams=c2d6d0b4:1 # NU5 (Orchard)
nuparams=c8e71055:1 # NU6
nuparams=4dec4df0:1 # NU6_1 https://zips.z.cash/zip-0255#nu6.1deployment

### MetaData Storage and Retrieval
# txindex:
# https://zcash.readthedocs.io/en/latest/rtd_pages/zcash_conf_guide.html#miscellaneous-options
txindex=1
# insightexplorer:
# https://zcash.readthedocs.io/en/latest/rtd_pages/insight_explorer.html?highlight=insightexplorer#additional-getrawtransaction-fields
insightexplorer=1
experimentalfeatures=1
lightwalletd=1

### RPC Server Interface Options:
# https://zcash.readthedocs.io/en/latest/rtd_pages/zcash_conf_guide.html#json-rpc-options
rpcuser=xxxxxx
rpcpassword=xxxxxx
rpcport=8232
rpcallowip=127.0.0.1

# Buried config option to allow non-canonical RPC-PORT:
# https://zcash.readthedocs.io/en/latest/rtd_pages/zcash_conf_guide.html#zcash-conf-guide
listen=0

i-am-aware-zcashd-will-be-replaced-by-zebrad-and-zallet-in-2025=1

### Zcashd Help provides documentation of the following:
mineraddress=uregtest1zkuzfv5m3yhv2j4fmvq5rjurkxenxyq8r7h4daun2zkznrjaa8ra8asgdm8wwgwjvlwwrxx7347r8w0ee6dqyw4rufw4wg9djwcr6frzkezmdw6dud3wsm99eany5r8wgsctlxquu009nzd6hsme2tcsk0v3sgjvxa70er7h27z5epr67p5q767s2z5gt88paru56mxpm6pwz0cu35m
minetolocalwallet=0 # This is set to false so that we can mine to a wallet, other than the zcashd wallet.
```

### Example: Zcashd Config File

```
grpc-bind-addr: 127.0.0.1:9067
cache-size: 10
log-file: /home/user/tmp/lwd_regtest/logs/lwd.log
log-level: 10
zcash-conf-path: /home/user/tmp/zcashd_regtest/zcash.conf
```

## Wallet Encryption (at rest)

By default the wallet file (`zingo-wallet.dat`) is stored unencrypted, so its protection
relies on your operating system (file permissions, full-disk encryption, and so on). You can
optionally encrypt the wallet file with a passphrase so the file on disk is useless to anyone
who obtains it.

### What this protects (and what it doesn't)

It protects the wallet file at rest: a stolen or lost device backup, a synced cloud backup, a
shared filesystem, a discarded disk. Because the whole file is encrypted, it also hides
privacy metadata such as your addresses, transaction history, and balances.

It does not protect a wallet that is currently open. While zingo-cli is running, the keys are
decrypted in memory so the wallet can sync and sign, which is unavoidable for any hot wallet.
It also can't help anyone who already has your passphrase.

There is no passphrase recovery. If you forget the passphrase the wallet file cannot be
opened, and the funds are unrecoverable unless you still have the seed phrase. Back up your
seed phrase separately.

### Supplying a passphrase

A passphrase can be provided three ways, listed most secure first:

1. Interactive prompt. If you open an encrypted wallet without supplying a passphrase,
   zingo-cli prompts for it without echoing to the terminal.
2. The `ZINGO_PASSPHRASE` environment variable.
3. The `--passphrase` flag. Avoid this on shared machines, since the value can leak via the
   process list (`ps`) and your shell history.

### Creating a new encrypted wallet

Pass a passphrase when creating a wallet (fresh, from `--seed`, or from `--viewkey`). The
wallet file is encrypted from its first save:

```bash
# Fresh wallet, encrypted (passed via env var to keep it out of shell history)
ZINGO_PASSPHRASE='your secret passphrase' ./target/release/zingo-cli

# Restore from seed, encrypted
ZINGO_PASSPHRASE='your secret passphrase' \
  ./target/release/zingo-cli --seed "word1 word2 ... word24" --birthday 600000
```

By default the key-derivation function uses 64 MiB of memory. On a memory-constrained device
you can lower this when creating the wallet with `--kdf-memory-mib` (range 1 to 256). Higher
is harder to crack but slower to open:

```bash
ZINGO_PASSPHRASE='your secret passphrase' \
  ./target/release/zingo-cli --kdf-memory-mib 32
```

The chosen value is recorded in the wallet file, so opening it later needs no flag.
`--kdf-memory-mib` only affects newly created wallets.

### Opening an encrypted wallet

Start zingo-cli pointing at the encrypted wallet's data directory. If the file is encrypted
and you don't pass a passphrase, you are prompted:

```bash
./target/release/zingo-cli --data-dir /path/to/wallet
# -> Wallet is encrypted. Enter passphrase:
```

Or supply it non-interactively:

```bash
ZINGO_PASSPHRASE='your secret passphrase' \
  ./target/release/zingo-cli --data-dir /path/to/wallet
```

An unencrypted wallet opens as before, with no passphrase requested.

### Managing encryption from inside the CLI

Two interactive commands operate on the currently open wallet. The change is written to disk
on the next save. The background save task runs automatically (roughly once per second), so
it is persisted shortly after the command returns.

`encrypt` encrypts a previously unencrypted wallet, or rotates to a new passphrase if it is
already encrypted (a new random salt is generated, fully re-keying the file). It always
prompts for the passphrase twice (no echo) and requires the two entries to match, so a typo
can't silently lock you out and the passphrase never lands in your session history. An
optional `--kdf-memory-mib <MIB>` flag sets the key-derivation memory (default 64).

`decrypt` disables encryption and writes the wallet in the clear from the next save onward.
Only do this if the file is protected by other means.

```text
(main) Block:... >> encrypt
New passphrase:
Confirm passphrase:
Wallet encryption enabled. The encrypted wallet will be saved shortly.

(main) Block:... >> encrypt --kdf-memory-mib 32   # prompts, then uses 32 MiB
(main) Block:... >> decrypt
Wallet encryption disabled. The wallet will be saved in the clear shortly.
```

### How it works (technical)

The entire serialized wallet is wrapped in a single authenticated-encryption envelope before
being written to disk.

Argon2id derives a 256-bit key from your passphrase and a random per-wallet salt. This runs
once when the wallet is opened or re-encrypted, and the derived key is then cached in memory,
so the routine per-second save loop only pays for the cheap symmetric step. The cost
parameters (memory, defaulting to 64 MiB, plus 3 iterations and 1 lane) are stored in the
header and reused on open.

The payload is encrypted with XChaCha20-Poly1305 (AEAD) using a fresh random nonce for every
save. The envelope header (format version, KDF parameters, salt, nonce) is stored in the
clear and authenticated as associated data, so the file is self-describing and tamper-evident.

The KDF parameters live in the clear header and must be read to derive the key before the
AEAD can authenticate them, so they are bounds-checked against sane limits first. A tampered
file therefore can't request an enormous amount of memory and crash the process on open.

Existing unencrypted wallet files continue to load unchanged. An encrypted file is identified
by a magic prefix that can never be confused with a plaintext wallet.

## Exiting the CLI

To quit the Zingo CLI, use the `quit` command (not `exit`).

**Note:** Each network (mainnet, testnet, regtest) requires its own wallet data. If you get an error about wallet chain name mismatch, ensure you're using the correct data directory for your chosen network.
