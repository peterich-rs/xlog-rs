# mars-xlog-cli

Install the Mars xlog decoder with npm:

```bash
npm install -g mars-xlog-cli
mars-xlog decode --input app.xlog --output app.log --key <64-hex-private-key>
```

`--key` is only required for encrypted async xlog blocks.
