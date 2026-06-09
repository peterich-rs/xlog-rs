# mars-xlog-cli

Command-line decoder for Tencent Mars `xlog` files.

```bash
mars-xlog decode --input app.xlog --output app.log --key <64-hex-private-key>
```

`--key` is only required for encrypted async xlog blocks. Plaintext files can be
decoded with `--input` and `--output` only.

The command also accepts the short flags used by the historical GUI wrapper:

```bash
mars-xlog decode -i app.xlog -o app.log -p <64-hex-private-key>
```

Release builds publish prebuilt archives for Homebrew/npm installers:

- `mars-xlog-v<version>-aarch64-apple-darwin.tar.gz`
- `mars-xlog-v<version>-x86_64-apple-darwin.tar.gz`
- `mars-xlog-v<version>-x86_64-unknown-linux-gnu.tar.gz`
- `mars-xlog-v<version>-x86_64-pc-windows-msvc.tar.gz`
