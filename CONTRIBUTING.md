# Contributing to honeybeepf-llm

First off, thank you for considering contributing to honeybeepf-llm! It's people like you that make honeybeepf-llm such a great tool.

## Code of Conduct

This project and everyone participating in it is governed by our [Code of Conduct](CODE_OF_CONDUCT.md). By participating, you are expected to uphold this code.

## How Can I Contribute?

### Reporting Bugs

Before creating bug reports, please check the existing issues as you might find out that you don't need to create one. When you are creating a bug report, please include as many details as possible:

- **Use a clear and descriptive title** for the issue to identify the problem.
- **Describe the exact steps which reproduce the problem** in as many details as possible.
- **Provide specific examples to demonstrate the steps**.
- **Describe the behavior you observed after following the steps** and point out what exactly is the problem with that behavior.
- **Explain which behavior you expected to see instead and why.**
- **Include your environment details** (OS, kernel version, etc.)

### Suggesting Enhancements

Enhancement suggestions are tracked as GitHub issues. Create an issue and provide the following information:

- **Use a clear and descriptive title** for the issue to identify the suggestion.
- **Provide a step-by-step description of the suggested enhancement** in as many details as possible.
- **Provide specific examples to demonstrate the steps**.
- **Describe the current behavior** and **explain which behavior you expected to see instead** and why.
- **Explain why this enhancement would be useful** to most honeybeepf-llm users.

### Pull Requests

1. Fork the repo and create your branch from `main`.
2. If you've added code that should be tested, add tests.
3. If you've changed APIs, update the documentation.
4. Ensure the test suite passes.
5. Make sure your code follows the existing code style.
6. **Run the pre-PR checklist below** before submitting.
7. Issue that pull request!

### Pre-PR Checklist

Before submitting a pull request, please run the following commands:

```bash
# Format code
make fmt

# Check formatting (CI will fail if this fails)
make fmt-check

# Run linter
make lint

# Run tests
make test
```

All checks must pass before your PR can be merged.

## Development Setup

### Prerequisites

- Rust toolchain (see `rust-toolchain.toml`)
- Linux kernel with eBPF support (4.18+)
- bpf-linker for eBPF compilation

### Building

```bash
make build
```

### Running Tests

```bash
make test
```

## Styleguides

### Git Commit Messages

- Use the present tense ("Add feature" not "Added feature")
- Use the imperative mood ("Move cursor to..." not "Moves cursor to...")
- Limit the first line to 72 characters or less
- Reference issues and pull requests liberally after the first line

### Rust Styleguide

- Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Run `cargo fmt` before committing
- Ensure `cargo clippy` passes without warnings

## Project Governance

honeybeepf-llm is maintained by the core team listed in the README. Decisions about the project direction are made through:

1. **GitHub Issues** - For feature requests and bug reports
2. **Pull Requests** - For code contributions
3. **Discussions** - For broader conversations about the project

All contributions are welcome, and maintainers will review and provide feedback on all submissions.

For additional details, review the [Governance Model](GOVERNANCE.md).

## License

By contributing, you agree that your contributions will be licensed under the project's licenses:

- **Non-eBPF code**: [MIT License](LICENSE-MIT) or [Apache License 2.0](LICENSE-APACHE)

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you shall be licensed as above, without any additional terms or conditions.
