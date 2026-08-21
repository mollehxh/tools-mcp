# Updating the pinned Codex compatibility baseline

The current authority is OpenAI Codex commit
`8cabf5a6cf103cebe338d46346e43e3201e64f41`. An upstream update is a deliberate
compatibility release, not an automatic dependency bump.

1. Audit the candidate Codex commit and update only separable unchanged modules
   or narrowly traced adaptations.
2. Update `third_party/openai-codex/SOURCE.toml`, retained `LICENSE`/`NOTICE`,
   source hashes, and `MODIFICATIONS.md` classifications.
3. Regenerate frozen schemas/results only when the pinned contract actually
   changed; register every intentional MCP or fixed-workspace difference.
4. Re-audit the macOS Seatbelt policy and marker provenance. Increment the
   sandbox capability protocol for an incompatible policy/launcher change.
5. Review `Cargo.lock` license metadata and refresh `THIRD_PARTY_NOTICES.md`.
6. Run `cargo run -p xtask -- upstream-verify`, the focused compatibility
   suites, shared conformance, package, and Inspector smoke on native macOS.
7. Repeat the disposable-profile ChatGPT/ngrok checklist and record a new
   release observation without overwriting older client-policy observations.

Do not claim Linux or Windows support during this process until their native
backends, packaging, conformance, and CI proofs are separately delivered.
