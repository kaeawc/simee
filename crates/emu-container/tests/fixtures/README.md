# Fixtures

`hello-world-oci/` is an OCI image layout exactly as `skopeo copy --quiet
docker://docker.io/library/hello-world@sha256:d1a8d0a4eeb63aff09f5f34d4d80505e0ba81905f36158cc3970d8e07179e59e
oci:hello-world-oci:15.0.0_64only-250627` wrote it (skopeo 1.4.1, 2026-11-28; the tag is the one
`image::pull_command` uses). It is the smallest real layout there is (about 4 KB, one 2.4 KB layer),
so the tests that check a layout against a pin read what the real tool produces rather than a file
written from a description. The pin the tests use is this image's amd64 manifest and its layer.
