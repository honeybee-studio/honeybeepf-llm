# Verify the image and chart

Images built from `main`, release images and the release chart are signed with keyless
[cosign](https://docs.sigstore.dev/cosign/) by the GitHub Actions workflow that built them, and the images carry an SBOM
and build provenance.

- The first fully signed release is 0.3.1; the 0.3.0 chart and images published before signing started are not signed.
- The signatures are stored in cosign 3's bundle format, so use cosign 3 or later: cosign 2 reports
  `no signatures found`.

To check a release before installing it, replace `X.Y.Z` with its version:

```bash
ID='^https://github.com/honeybee-studio/honeybeepf-llm/\.github/workflows/'
ISSUER=https://token.actions.githubusercontent.com
cosign verify ghcr.io/honeybee-studio/honeybeepf-llm:X.Y.Z \
  --certificate-identity-regexp "$ID" --certificate-oidc-issuer "$ISSUER"
cosign verify ghcr.io/honeybee-studio/charts/honeybeepf-llm:X.Y.Z \
  --certificate-identity-regexp "$ID" --certificate-oidc-issuer "$ISSUER"
docker buildx imagetools inspect ghcr.io/honeybee-studio/honeybeepf-llm:X.Y.Z --format '{{ json .SBOM }}'
```

The [security review](security-review.md#7-supply-chain) covers the rest of the supply chain: pinned actions, the
dependency audit and how binary files in the repository are checked.
