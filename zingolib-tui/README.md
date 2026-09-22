# zingolib-tui

A watch-only terminal wallet for Zcash. It imports a unified full viewing key, syncs
continuously, and shows balances, transactions with memos, and receiving addresses with QR
codes. It never holds a spending key and cannot spend.

```text
cargo run --release -p zingolib-tui
```

The first launch asks for the viewing key, whose prefix names the network, then the server, the birthday height, and an optional
passphrase for the wallet file. Later launches open the saved wallet. Pass `--ufvk` and
`--birthday` to skip the first-run screen, or set `ZINGOLIB_TUI_PASSPHRASE` for unattended
unlocks. See `--help` for the rest.

The screens are Home, Transactions, Receive, Addresses, and Sync. Each opens with the first
letter of its name, from anywhere: `h` `t` `r` `a` `s`. `j` `k` move, `Enter` opens, `Esc`
goes back, `c` picks a colour theme, `?` lists everything, and `q` quits after saving.

## Themes

Nord, Catppuccin (Latte, Frappé, Macchiato, Mocha), Vitesse (Dark, Light, Black), Vercel
(Dark, Light), Gruvbox (Dark, Light), your terminal's own colours, and monochrome. Press `c`
to open the picker: moving previews the theme live, `Enter` keeps and saves it, `Esc` reverts.

The choice is saved in `zingolib-tui/config` under the platform configuration directory. The
starting theme is the first of `--theme`, the saved choice, monochrome when `NO_COLOR` is set,
and Gruvbox Dark.

Colours are 24-bit when `COLORTERM` says the terminal supports it, and mapped to the nearest
of the 256 xterm colours otherwise. Force either with `--color-depth 24bit` or
`--color-depth 256`. QR codes use each theme's own dark and light tones, pushed apart until
they reach at least 12:1 contrast, so they match the theme and still scan.

Logs go to `zingolib-tui.log` in the data directory. `RUST_LOG` sets the level.

## Tests

```text
cargo test -p zingolib-tui
```

The tests need no wallet, no terminal and no network. One of them drives the backend against
zingolib's in-process mock indexer.
