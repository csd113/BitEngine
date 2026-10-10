# Rust and Clippy policy

This policy was audited against installed Rust/Clippy **1.99.0** on 2026-10-10.
[Cargo.toml](../Cargo.toml) and [clippy.toml](../clippy.toml) are authoritative;
[development standards](../CONTRIBUTING.md) describe the required workflow.

## Scope and compiler policy

BitEngine is one first-party executable with 18 Rust source files. It is not a
multi-crate workspace. The patched `vendor/tor-hsservice` is third-party Arti
0.47.0 source with separate provenance and policy. Neither its source nor either
lockfile was changed by this cleanup. There are no mutually exclusive package
features and no separate library/doctest target.

Rust `warnings`, `future_incompatible`, and `rust_2018_idioms` are denied at
priority -1. Individual lints use the stronger default priority 0:

- `dead_code_pub_in_binary`, `unreachable_pub`, `unused_lifetimes`, and
  `unused_must_use` keep the executable's internal interfaces intentional.
- `missing_debug_implementations`, `missing_docs`, and `unexpected_cfgs` protect
  API and build-configuration hygiene. Credential-bearing types must implement
  redacted Debug rather than deriving it.
- `unsafe_op_in_unsafe_fn` and `raw_borrows_via_references` make unsafe operations
  explicit and reject references created merely to obtain raw pointers when
  reference validity cannot be established.

The repository already pinned 1.99.0 before this work; the previous compiler for
this audit was also 1.99.0. The manifest now spells the MSRV as `1.99.0`, scripts
reject compiler/Cargo overrides, and pinned CI validates every supported target.
Edition 2021 and application version 1.0.0 are unchanged. The earlier compiler
and dependency upgrades remain recorded in [the upgrade history](rust-1.99-upgrade.md).

The [Rust release notes](https://doc.rust-lang.org/releases.html) and installed
lint inventories were reviewed. `raw_borrows_via_references` is enabled without
exceptions; existing OS calls already use raw output storage with justified
initialization. `String::from_utf8_lossy_owned` now reuses owned node-output
buffers when UTF-8 is valid, preserving replacement behavior for invalid bytes.
No unrelated language/edition migration or dependency update was necessary.

## Clippy groups and selected checks

`all`, `pedantic`, `nursery`, and `cargo` are denied at priority -1. Individual
rules are denied at priority 0. `restriction` itself is never enabled: its
contradictory style and platform policies cannot form a coherent blanket rule.
The installed 1.99.0 catalogue was checked before selecting names.

| Selection | Reason |
|---|---|
| `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `unreachable`, `exit` | Recoverable failures must return context and unwind owned resources. The duplicate-instance entry path returns success normally. |
| `missing_assert_message` | Invariants using an assertion need an actionable diagnostic. |
| `dbg_macro`, `print_stdout`, `print_stderr` | Native GUI diagnostics must reach bounded logs or explicit error presentation. |
| `indexing_slicing`, `string_slice` | Input-derived bounds must be checked; compile-time safe array ranges remain accepted by Clippy. |
| `as_conversions`, `cast_possible_truncation`, `cast_possible_wrap`, `cast_sign_loss`, `cast_precision_loss`, `as_pointer_underscore`, `as_underscore`, `fn_to_numeric_cast_any` | Conversions and pointer types must expose their range/representation contract. |
| `undocumented_unsafe_blocks`, `multiple_unsafe_ops_per_block`, `unnecessary_safety_comment`, `default_union_representation` | Minimize unsafe regions, require real invariants, and avoid unspecified union layouts. |
| `host_endian_bytes`, `precedence_bits`, `suspicious_xor_used_as_pow` | Wire formats must be portable; bitwise expressions must have unambiguous intent. |
| `pointer_format` | Memory addresses are not useful application diagnostics. |
| `clone_on_ref_ptr`, `mem_forget`, `rc_mutex` | Make shared ownership explicit and prevent leaked resources or incoherent synchronization. |
| `unused_result_ok`, `unused_trait_names` | Preserve failure handling and avoid importing unneeded trait names. |
| `map_with_unused_argument_over_ranges`, `pathbuf_init_then_push` | Avoid needless iteration and path-construction work. |
| `tests_outside_test_module` | Keep fixtures and production policy boundaries visible. |
| `cognitive_complexity`, `large_stack_arrays`, `large_stack_frames` | Limit control-flow complexity and stack pressure in GUI/background work. |
| `allow_attributes`, `allow_attributes_without_reason` | Use justified expectations that detect when their exemption becomes obsolete. |
| `missing_errors_doc`, `missing_panics_doc`, `missing_safety_doc` | Document exported failure and safety contracts. Shared internal fallible APIs also receive meaningful error documentation during review. |

`arithmetic_side_effects` is additionally denied in `binaries` (including all its
children), `bitcoin_config`, `bitcoin_status`, `config`, `rpc`, `electrs_status`,
`process_manager`, and `tor`. Sizes, counters, PID negation, capacity calculations,
and deadlines require explicit checked/saturating behavior. The GUI's geometry
and display percentages do not inherit a blanket arithmetic ban; their casts
still require the global conversion checks and local display-only invariants.

### Configuration decisions

- Function length 100, arguments 7, cognitive complexity 25, type complexity 250,
  enum size difference 200 bytes, and stack size 512,000 bytes are explicit.
  Existing linear transaction/teardown expectations remain local instead of
  raising limits across the application. No disallowed types are needed:
  ownership-sensitive checks and the existing APIs provide a more precise rule.
- `std::env::set_var` and `remove_var` are disallowed. Build settings belong to
  the child `Command` environment, avoiding races with application threads.
- Fixture-only `expect`, `unwrap`, and indexing are permitted through the three
  dedicated `allow-*-in-tests` settings. Production restrictions remain denied;
  assertions, rather than recovered application errors, are the fixture contract.
- `check-incompatible-msrv-in-tests = true` also keeps fixture APIs on 1.99.0.
- `check-private-items = false` keeps Errors/Panics linting on exported APIs.
  A trial enabling it produced 444 missing-Errors and 291 missing-Panics reports
  across private helpers/fixtures. Boilerplate on every Result-returning test
  would obscure its assertion contract. This does not remove the review rule to
  document shared fallible operations, and rustdoc builds private documentation.

### Remaining restriction catalogue decisions

Every restriction name from the installed toolchain is either selected above,
applied per module, or accounted for below. Rejection is based on semantics,
scope, or conflicting requirements, rather than the cost of fixing diagnostics.

| Not globally selected | Reason |
|---|---|
| `absolute_paths`, `arbitrary_source_item_ordering`, `definition_in_module_root`, `field_scoped_visibility_modifiers`, `pub_use`, `pub_with_shorthand`, `pub_without_shorthand` | Existing module/re-export boundaries and narrow visibility communicate intent. Shorthand alternatives conflict; mandated ordering/path spelling adds unrelated churn. |
| `inline_modules`, `mod_module_files`, `self_named_module_files`, `multiple_inherent_impl`, `inline_trait_bounds`, `impl_trait_in_params` | File naming alternatives conflict, and separate impl/test modules are useful organization rather than a safety defect. |
| `implicit_return`, `question_mark_used`, `if_then_some_else_none`, `else_if_without_else`, `return_and_then`, `try_err`, `wildcard_enum_match_arm` | Idiomatic tail expressions, `?`, conditional Options, early error returns, and partial state matches with intentional fallbacks are appropriate. Mandatory alternative control flow would make error handling less direct. |
| `semicolon_inside_block`, `semicolon_outside_block`, `separated_literal_suffix`, `unseparated_literal_suffix`, `needless_raw_strings`, `redundant_type_annotations`, `ref_patterns`, `unneeded_field_pattern`, `rest_pat_in_fully_bound_structs`, `rest_pattern_accessible_field`, `unnecessary_rest_pattern` | Several rules are opposing style choices. Ordinary rustfmt and pedantic checks provide consistency without rewriting clear patterns. |
| `shadow_reuse`, `shadow_same`, `shadow_unrelated`, `single_call_fn`, `same_name_method`, `renamed_function_params`, `single_char_lifetime_names`, `min_ident_chars`, `module_name_repetitions`, `pattern_type_mismatch` | Refinement shadowing, named lifecycle helpers, conventional lifetimes, and match ergonomics are intentional. These blanket naming/structure rules do not improve ownership or correctness here. |
| `exhaustive_enums`, `exhaustive_structs`, `partial_pub_fields`, `missing_inline_in_public_items`, `error_impl_error`, `missing_trait_methods` | This executable has no external library API. Internal exhaustive state machines, protected fields, typed Error implementations, and trait defaults are useful. |
| `missing_docs_in_private_items`, `doc_paragraphs_missing_punctuation`, `doc_include_without_cfg`, `unnecessary_safety_doc` | Require meaningful shared contracts and checked rustdoc; do not demand prose on every fixture/private field, prohibit useful invariant docs, or enforce punctuation conventions. There are no included-document feature contracts. |
| `cfg_not_test`, `assertions_on_result_states`, `missing_asserts_for_indexing`, `get_unwrap`, `unwrap_in_result`, `panic_in_result_fn`, `redundant_test_prefix` | Real OS/process code needs test substitutions. Result-state assertions and descriptive fixture names are valid. Production panic/index restrictions already apply; `panic_in_result_fn` also rejects legitimate assertions in Result-returning tests. |
| `float_arithmetic`, `float_cmp_const`, `integer_division`, `integer_division_remainder_used`, `modulo_arithmetic`, `lossy_float_literal`, `default_numeric_fallback`, `decimal_literal_representation` | Geometry needs floating-point arithmetic; binary protocols and byte-unit formatting need exact integer quotients, remainders, and explicit endianness. Correctness-sensitive arithmetic is checked by the module policy. |
| `big_endian_bytes`, `little_endian_bytes` | Both representations have legitimate protocol uses; banning either is incompatible with portable wire formats. Host-endian conversion is separately denied. |
| `inline_asm_x86_att_syntax`, `inline_asm_x86_intel_syntax` | Mutually exclusive x86 assembly preferences; first-party code has no assembly. |
| `create_dir`, `filetype_is_file`, `map_err_ignore`, `let_underscore_must_use`, `let_underscore_untyped` | Exclusive directory creation and regular-file checks are security requirements. Reviewed cleanup/probe paths intentionally classify or discard best-effort results; prohibiting these constructs globally would obscure the ownership/failure contract. New ignored failures still require review. |
| `empty_drop`, `empty_enum_variants_with_brackets`, `empty_structs_with_brackets`, `infinite_loop`, `iter_over_hash_type`, `large_include_file`, `verbose_file_reads` | Empty-shape preferences and API spelling are not safety properties. Supervisors may run until cancellation, map iteration need not always be ordered, and bounded configuration reads are reviewed directly. No large embedded file requires a new limit. |
| `mutex_atomic`, `mutex_integer`, `non_zero_suggestions`, `rc_buffer` | Synchronization and validated integer representation depend on ownership/compound invariants. Blanket type substitutions can change cancellation/locking semantics without benefit. |
| `alloc_instead_of_core`, `std_instead_of_alloc`, `std_instead_of_core`, `str_to_string`, `string_add`, `string_lit_chars_any`, `deref_by_slicing` | This is a std desktop executable; no no_std portability contract exists. Conventional string/slice operations are clearer when their bounds and ownership are already checked. |
| `non_ascii_literal`, `disallowed_script_idents`, `unnecessary_self_imports`, `use_debug`, `mixed_read_write_in_expression` | UI text intentionally uses Unicode. Existing import rules and compiler identifier warnings suffice. Debug remains useful for typed diagnostic state, with explicit credential redaction. Expressions are reviewed for actual borrowing/evaluation hazards. |

## Suppression audit

There are no first-party `allow` attributes. Every remaining `expect` has a
reason and is checked for fulfillment. No command-line `-A` or CI skip was added.

Removed or consolidated:

- Two obsolete `too_many_arguments` allowances on the seven-argument Bitcoin
  and electrs builders: both pass the unchanged threshold.
- The UI `private_interfaces` expectation: Iced accepts the internal Message
  type, so corrected visibility resolves the mismatch.
- Single-instance listener `dead_code`: the `_listener` field now explains that
  its ownership keeps the guard alive; the socket is still held until drop.
- The Tor module's `redundant_pub_crate` expectation is consolidated with the
  executable-wide compiler/Clippy visibility conflict below.

Retained and newly required expectations:

| Scope | Lint and justification |
|---|---|
| `main` | Existing `multiple_crate_versions`: `cargo tree --locked --duplicates` confirms upstream Iced/rfd/Arti splits (for example bitflags 1 via CoreGraphics and bitflags 2 via current GUI/Arti, and rand/rand_core generation splits). Forcing incompatible transitive versions is not a safe application cleanup. Re-review when dependencies change. |
| `main` | New single-lint `redundant_pub_crate` expectation: 1.99 Clippy suggests unrestricted `pub` under private modules, conflicting with denied rustc `unreachable_pub`. Explicit internal visibility is required throughout this executable. The [Clippy implementation](https://github.com/rust-lang/rust-clippy/blob/rust-1.99.0/clippy_lints/src/redundant_pub_crate.rs) confirms this predicate. No lint group is exempted. |
| `electrs_status::ElectrsStatus`, `ui::App` | Existing `struct_excessive_bools`: independent process, synchronization, RPC/P2P, and protocol-readiness facts must remain independently observable. |
| `binaries::process::run` (fixture wrapper) | Existing `too_many_arguments`: explicit identity, output channels, environment, and cancellation inputs make process tests understandable. |
| `BuildService::run`, `run_pipeline`, `prepare_source` | Existing `too_many_lines`: linear preflight, authentication, transaction, durable status, and cleanup ordering remain reviewable in one place. |
| `tor::run_arti_attempt` | Existing `too_many_lines`: one cancellation-aware owner keeps Arti teardown ordering explicit. |
| `tor::run_electrum_proxy_session` | Existing `too_many_arguments`: independently borrowed lifecycle resources preserve ownership and testability. |
| `tor::bootstrap_percent` | Existing truncation/sign-loss expectations plus new `as_conversions`: the value is clamped to the u8 percentage range before a display conversion. |
| `ui_render::connection_progress` | Existing truncation expectation plus new `as_conversions`: bounded display-only f64 percentages feed Iced's f32 progress widget. |
| Tor identity fixture | Existing `case_sensitive_file_extension_comparisons`: `.onion` is a canonical lowercase hostname suffix, not a filesystem extension. |

## Operational and unsafe-code review

The complete first-party module set and diff were reviewed against these
boundaries; the cleanup does not replace transaction, authentication, or task
ownership architecture:

- Build commands keep explicit argument vectors and per-child environments.
  Reader tasks, cancellation guards, bounded output channels, process-group
  termination, and reaping remain owned. Overflowing grace deadlines now skip
  directly to cleanup instead of panicking.
- Source origins, release/signer policy, signed tags, clean trees, every-output
  version verification, destination locks, staging, Prepared/Committed journals,
  backup/activation ordering, fsync, rollback, and fail-closed recovery remain in
  place. No tests touch real node data or custom installations.
- Configuration and cookie reads retain bounds, no-follow checks, regular-file
  checks, and generation snapshots. Serialization formats are unchanged.
- Status polling/Tor commands retain generation/identity checks, bounded work,
  cancellation, and readiness gating. Iced's update dispatcher already delegates
  to lifecycle/preferences/build handlers; further splitting would add indirection.
- RPC response bodies now have a 1 MiB cap for declared and streamed lengths.
  Existing five-second timeout, authentication, warmup, and protocol semantics
  are protected by loopback tests. RpcAuth's Debug hides username/password.
  Managed RPC explicitly disables inherited proxies; a subprocess fixture
  verifies that hostile proxy variables cannot redirect RPC credentials.
- Durable build-log finalization now reports I/O and task-join errors to the UI.
  A post-commit log failure remains a warning on a successful installation;
  failed/cancelled builds retain their original classification and diagnostic.
- Release parsing avoids temporary collections, node decoding reuses owned
  buffers, QR run traversal uses slices, and Iced task batching avoids an extra
  intermediate Vec while preserving task construction order.

Unsafe inventory: production calls are `geteuid` (environment/dependency/Tor
ownership), `flock` (workspace/destination locks), `statvfs` plus initialized
storage (free space), `kill` (owned child/process groups), `waitid` plus
`siginfo_t` access (observe without reaping), and `poll` (live reader descriptor).
Fixtures additionally query/signal their own children and call `waitpid` to
verify reaping. Every block has a safety invariant. There are no first-party
unsafe functions, unsafe impls, or inline assembly. The `waitid` initialization
and union access now occupy separate minimal unsafe blocks. No unsafe code was
introduced for performance.

## Enforcement

[The shared gate](../scripts/quality-gate.sh) verifies exact versions, formatting,
all-target/all-feature compilation, strict Clippy, tests, private rustdoc with
warnings denied, and release compilation. [CI](../.github/workflows/ci.yml) runs
it on native macOS ARM64, Linux x86_64, and Linux ARM64. Release jobs run the same
gate before packaging. A supplementary stable job preserves future-compiler
early warnings and cannot replace pinned validation. See the dated validation
record in [the upgrade history](rust-1.99-upgrade.md) for actual local results.
