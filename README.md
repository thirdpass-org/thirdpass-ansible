# thirdpass-ansible

Ansible package extension for Thirdpass.

This repo contains the Thirdpass extension that understands Ansible
Galaxy collections and Ansible dependency files. It can be used by the
Thirdpass CLI to discover Ansible dependencies and fetch package
metadata from Ansible Galaxy.

## Dependency-tree review

`thirdpass review <namespace.collection> <version> --deps --extension
ansible` resolves collection dependencies from Ansible Galaxy
metadata. The extension selects the newest Galaxy collection version
that satisfies each dependency requirement, then recursively walks
those dependencies with a per-run cache for collection metadata and
version listings.

This is dependency discovery for review, not a full `ansible-galaxy
collection install` solver. If different dependency branches require
incompatible versions of the same collection, the extension resolves
each dependency edge to the newest satisfying version it sees. That is
useful for review candidate discovery, but it is not guaranteed to
match every install plan that `ansible-galaxy` could produce.

Galaxy versions that cannot be parsed as semver-compatible collection
versions are skipped during version selection and logged at debug level.

## Install

Install the extension as a normal Cargo binary:

```bash
cargo install thirdpass-ansible
```

Ensure Cargo's binary directory, usually `~/.cargo/bin`, is on `PATH`,
then verify Thirdpass can discover the extension:

```bash
thirdpass extension list
```
