# Paseo Docker Image

This directory builds the native Rust daemon/CLI and its runtime assets. The
base image has no Node executable or bundled web UI; connect with a native
Flutter client or the CLI, not a browser page at the daemon's HTTP root.

Build and run instructions are in [docs/docker.md](../docs/docker.md). Do not
assume an existing registry `latest` image contains this implementation.

Agent CLIs are optional, separately installed dependencies. The explicit
`Dockerfile.agents.example` shows how to add providers that require Node without
adding it to the default Paseo image. Backend plugins use native executables.

The Docker workflow is manual-only. Commit and tag pushes neither build nor
publish images; publication requires an explicit matching version and opt-in.
