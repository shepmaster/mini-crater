# mini-crater

This tool finds the crates that rely on a target crate and builds them
locally with an updated version of the target crate. This can be used
to evaluate the scope of a breaking change before it is released.

# Usage

1. Install the tool

    ```
    cargo install --git https://github.com/shepmaster/mini-crater
    ```

1. Run the tool. Provide the crate name, the version to upgrade from,
   and one or more patch lines to substitute in. You can set the
   environment variable `RUST_LOG=trace` to have more detail about the
   process.

# Examples

## Normal upgrade

This evaluates crates that use SNAFU 0.6 to see if they will all build
with a locally-modified version of `snafu` and `snafu-derive`.

```
mini-crater snafu \
  --version 0.8.9 \
  --patch 'snafu = { path = "/Users/shep/Projects/snafu" }' \
  --patch 'snafu-derive = { path = "/Users/shep/Projects/snafu/snafu-derive" }'
```

## Applying changes along with the upgrade

This runs a user-specified script called `adjust-code.sh` as the
second build step:

```
mini-crater snafu \
  --version 0.8.9 \
  --patch 'snafu = { path = "/Users/shep/Projects/snafu" }' \
  --patch 'snafu-derive = { path = "/Users/shep/Projects/snafu/snafu-derive" }' \
  --use-git \
  --post-command /tmp/adjust-code.sh
```

User-specified scripts have the following environment variables available:

- `MINI_CRATER_WORKING_DIR` - The root working directory.
- `MINI_CRATER_EXPERIMENT_NAME` - The name of the current crate being
  experimented on.
- `MINI_CRATER_EXPERIMENT_CHECKOUT_DIR` - When a git checkout has been
  performed, this points to the top of the checkout.
