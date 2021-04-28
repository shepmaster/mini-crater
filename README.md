# Example

```
RUST_LOG=trace \
cargo run -- \
snafu \
--version 0.6 \
--patch snafu=/Users/shep/Projects/snafu \
--patch snafu-derive=/Users/shep/Projects/snafu/snafu-derive \
--use-git \
--post-command apply-code-modifications.sh
```
