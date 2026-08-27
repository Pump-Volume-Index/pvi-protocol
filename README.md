# PVI Protocol

Public, reproducible Anchor source for the Pump Volume Index synthetic-token program.

- Program ID: `7PKXznczrtwCSSYqMEhgFjvJqxpnpgGMZUX4RTy3XVgb`
- Library name: `pvi`
- Anchor: `0.31.1`
- Verification mount path: repository root

The program initializes paused. Source verification proves deployed bytecode matches this repository; it is not a substitute for an independent security audit.

## Verifiable build

```sh
solana-verify build --library-name pvi
solana-verify get-executable-hash target/deploy/pvi.so
```

Deploy the exact `target/deploy/pvi.so` produced by the verifiable build, then compare it with the on-chain program hash before registering the repository and submitting remote verification.
