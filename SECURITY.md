# Security policy

Security fixes land on `main` and ship in the next release; there are no maintained release branches.

## Report a vulnerability

Use GitHub's [private vulnerability reporting form](https://github.com/Gondola-Bros-Entertainment/chunker/security/advisories/new)
to report suspected vulnerabilities. Include the version (`chunker --version`), platform, the command you ran,
reproduction steps and expected impact.

Keep exploit details and sensitive data out of public issues and pull requests. Do not include credentials, bucket
names or files you cannot share. The maintainers will review reports privately and coordinate fixes and disclosure
through GitHub security advisories.

## Scope

Reports may cover how chunker reads its input, the manifests and `latest.txt` it reads back from a bucket, storage
credentials, publication to S3 or R2, the release executables and their checksums, or repository automation. Ordinary
build failures and feature requests belong in public issues.
