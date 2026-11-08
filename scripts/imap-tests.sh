#!/usr/bin/env bash
# Runs the ignored IMAP tests against a local GreenMail server. Needs Java 21+.
# GreenMail is a test server with throw-away users; it never touches real mail.
set -euo pipefail

VERSION="2.1.15"
SHA256="91d58798698cac357f015365240383b936ced2f2a1a9f7db37d9e3e1a31391b1"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JAR="$ROOT/target/greenmail/greenmail-standalone-$VERSION.jar"
URL="https://repo1.maven.org/maven2/com/icegreen/greenmail-standalone/$VERSION/greenmail-standalone-$VERSION.jar"

sha256_of() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

if [ ! -f "$JAR" ]; then
  mkdir -p "$(dirname "$JAR")"
  curl --fail --silent --show-error --location "$URL" --output "$JAR.part"
  mv "$JAR.part" "$JAR"
fi

actual="$(sha256_of "$JAR")"
if [ "$actual" != "$SHA256" ]; then
  echo "GreenMail checksum mismatch: $actual" >&2
  rm -f "$JAR"
  exit 1
fi

java \
  -Dgreenmail.setup.test.imap \
  -Dgreenmail.setup.test.imaps \
  -Dgreenmail.setup.test.smtp \
  -Dgreenmail.hostname=127.0.0.1 \
  -Dgreenmail.users=invoices:test-password@example.test \
  -jar "$JAR" >"$ROOT/target/greenmail/server.log" 2>&1 &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true' EXIT

# GreenMail's test setup listens on 3025 (SMTP), 3143 (IMAP) and 3993 (IMAPS with
# a self-signed certificate). An open port is not enough: the test user can
# appear a moment later, so wait until it can log in.
login_works() {
  (
    exec 3<>/dev/tcp/127.0.0.1/3143 || exit 1
    IFS= read -r -t 5 _ <&3 || exit 1
    printf 'a1 LOGIN invoices test-password\r\n' >&3
    while IFS= read -r -t 5 line <&3; do
      case "$line" in
        "a1 OK"*) printf 'a2 LOGOUT\r\n' >&3; exit 0 ;;
        "a1 "*) exit 1 ;;
      esac
    done
    exit 1
  ) 2>/dev/null
}

ready=false
for _ in $(seq 1 60); do
  if login_works; then
    ready=true
    break
  fi
  sleep 1
done
if [ "$ready" != true ]; then
  echo "GreenMail did not accept the test user within 60 seconds. Server log:" >&2
  cat "$ROOT/target/greenmail/server.log" >&2
  exit 1
fi

status=0
DOCSWEEP_TEST_IMAP=127.0.0.1:3143 \
DOCSWEEP_TEST_IMAPS=127.0.0.1:3993 \
  cargo test -p docsweep-core --test imap -- --ignored --test-threads=1 || status=$?
if [ "$status" -ne 0 ]; then
  echo "IMAP tests failed. GreenMail server log:" >&2
  cat "$ROOT/target/greenmail/server.log" >&2
fi
exit "$status"
