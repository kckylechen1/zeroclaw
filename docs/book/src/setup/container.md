# Migrating from a product container

This fork no longer builds or publishes ZeroClaw product images. Install the native binary using the [platform setup guides](./index.md).

Before switching, back up the existing container's persistent data and configuration. Identify the host bind mount or named volume used for `/zeroclaw-data`, and preserve it until the native installation has been verified. Copy the configuration directory and data to a location accessible to the native service account, preserving ownership and permissions as appropriate. Point the native process at that configuration directory with `--config-dir` or `ZEROCLAW_CONFIG_DIR`, then verify the configuration, workspace and stored data before retiring the old deployment. Do not run both instances against the same data directory.

Docker remains available as an optional [tool-execution sandbox](../tools/python-skills.md); this is separate from packaging the ZeroClaw product.
