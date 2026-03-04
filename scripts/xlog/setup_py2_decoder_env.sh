#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Setup Python2 decoder environment for Phase 2C-2 (official crypt decode regression).

Usage:
  scripts/xlog/setup_py2_decoder_env.sh [--env-name <name>] [--python-version <version>] [--skip-openssl11]

Options:
  --env-name <name>          Pyenv virtualenv name (default: xlog-py2-decoder)
  --python-version <version> Python version for pyenv (default: 2.7.18)
  --skip-openssl11           Skip Homebrew openssl@1.1 install step
  -h, --help                 Show this help text
EOF
}

log() {
  printf '[setup] %s\n' "$*"
}

die() {
  printf '[setup] error: %s\n' "$*" >&2
  exit 1
}

python_version="2.7.18"
env_name="xlog-py2-decoder"
skip_openssl11=0

while (($# > 0)); do
  case "$1" in
    --env-name)
      env_name="${2:-}"
      shift 2
      ;;
    --python-version)
      python_version="${2:-}"
      shift 2
      ;;
    --skip-openssl11)
      skip_openssl11=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

if [[ -z "$env_name" ]]; then
  die "--env-name must not be empty"
fi
if [[ -z "$python_version" ]]; then
  die "--python-version must not be empty"
fi

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

ensure_pyenv() {
  if have_cmd pyenv; then
    return
  fi
  if ! have_cmd brew; then
    die "pyenv is missing and Homebrew is unavailable"
  fi
  log "installing pyenv + pyenv-virtualenv via Homebrew"
  brew install pyenv pyenv-virtualenv
}

ensure_pyenv

export PATH="$HOME/.pyenv/bin:$PATH"
eval "$(pyenv init -)"
eval "$(pyenv virtualenv-init -)"

install_openssl11() {
  if [[ "$skip_openssl11" == "1" ]]; then
    return
  fi
  if [[ "$(uname -s)" != "Darwin" ]]; then
    return
  fi

  if [[ -f "/opt/homebrew/opt/openssl@1.1/lib/libcrypto.dylib" || -f "/usr/local/opt/openssl@1.1/lib/libcrypto.dylib" ]]; then
    return
  fi

  if ! have_cmd brew; then
    die "openssl@1.1 not found and Homebrew unavailable"
  fi

  log "installing Homebrew openssl@1.1 (rbenv tap)"
  brew tap rbenv/tap >/dev/null
  brew install rbenv/tap/openssl@1.1
}

install_openssl11

download_python_source_if_needed() {
  local pyenv_root
  pyenv_root="$(pyenv root)"
  local cache_dir="${pyenv_root}/cache"
  local archive_name="Python-${python_version}.tar.xz"
  local archive_path="${cache_dir}/${archive_name}"

  mkdir -p "$cache_dir"
  if [[ -f "$archive_path" ]]; then
    return
  fi

  local urls=(
    "https://www.python.org/ftp/python/${python_version}/${archive_name}"
    "https://mirrors.huaweicloud.com/python/${python_version}/${archive_name}"
    "https://registry.npmmirror.com/-/binary/python/${python_version}/${archive_name}"
  )

  log "pre-downloading ${archive_name} to pyenv cache"
  local url
  for url in "${urls[@]}"; do
    if curl -fL --retry 3 --connect-timeout 15 "$url" -o "$archive_path"; then
      return
    fi
    log "download failed: ${url}"
  done

  die "unable to download ${archive_name} from all mirrors"
}

download_python_source_if_needed

build_with_brew_flags_if_available() {
  local cppflags_parts=()
  local ldflags_parts=()
  local formula

  if ! have_cmd brew; then
    CFLAGS="-Wno-error=implicit-function-declaration" pyenv install -s "$python_version"
    return
  fi

  for formula in readline zlib bzip2; do
    if brew --prefix "$formula" >/dev/null 2>&1; then
      local prefix
      prefix="$(brew --prefix "$formula")"
      cppflags_parts+=("-I${prefix}/include")
      ldflags_parts+=("-L${prefix}/lib")
    fi
  done

  local cppflags=""
  local ldflags=""
  if [[ "${#cppflags_parts[@]}" -gt 0 ]]; then
    cppflags="${cppflags_parts[*]}"
  fi
  if [[ "${#ldflags_parts[@]}" -gt 0 ]]; then
    ldflags="${ldflags_parts[*]}"
  fi

  CFLAGS="-Wno-error=implicit-function-declaration" \
  CPPFLAGS="$cppflags" \
  LDFLAGS="$ldflags" \
  PYTHON_BUILD_SKIP_MIRROR=1 \
  pyenv install -s "$python_version"
}

if ! pyenv versions --bare | rg -qx "$python_version"; then
  log "installing Python ${python_version} via pyenv"
  build_with_brew_flags_if_available
fi

if ! pyenv virtualenvs --bare | rg -qx "$env_name"; then
  log "creating virtualenv ${env_name}"
  pyenv virtualenv "$python_version" "$env_name"
fi

pyenv shell "$env_name"

log "installing Python package dependencies"
export PIP_DISABLE_PIP_VERSION_CHECK=1
pip install --upgrade "pip<21" "setuptools<45" wheel
pip install --no-cache-dir "pyelliptic==1.5.7" "zstandard==0.14.1"

site_packages="$(python - <<'PY'
from distutils.sysconfig import get_python_lib
print(get_python_lib())
PY
)"
if [[ -z "$site_packages" ]]; then
  die "failed to resolve python2 site-packages path"
fi

log "applying pyelliptic compatibility patch"
export XLOG_PY2_SITE_PACKAGES="$site_packages"
python3 - <<'PY'
from pathlib import Path
import os


def replace_once(text: str, old: str, new: str, *, label: str) -> str:
    if new in text:
        return text
    if old in text:
        return text.replace(old, new, 1)
    raise SystemExit("missing patch target: " + label)

site_packages = Path(os.environ["XLOG_PY2_SITE_PACKAGES"])
openssl_py = site_packages / "pyelliptic" / "openssl.py"
ecc_py = site_packages / "pyelliptic" / "ecc.py"

if not openssl_py.exists():
    raise SystemExit("pyelliptic openssl.py not found: " + str(openssl_py))
if not ecc_py.exists():
    raise SystemExit("pyelliptic ecc.py not found: " + str(ecc_py))

text = openssl_py.read_text()
text = replace_once(
    text,
    "import sys\nimport ctypes\nimport ctypes.util",
    "import sys\nimport os\nimport ctypes\nimport ctypes.util",
    label="import os",
)
text = replace_once(
    text,
    """        self.ECDH_OpenSSL = self._lib.ECDH_OpenSSL
        self._lib.ECDH_OpenSSL.restype = ctypes.c_void_p
        self._lib.ECDH_OpenSSL.argtypes = []
""",
    """        try:
            self.ECDH_OpenSSL = self._lib.ECDH_OpenSSL
        except AttributeError:
            # xlog-rs compatibility patch: OpenSSL 1.1+ removed method symbols.
            pass
        else:
            self._lib.ECDH_OpenSSL.restype = ctypes.c_void_p
            self._lib.ECDH_OpenSSL.argtypes = []
""",
    label="ECDH_OpenSSL compatibility",
)
text = replace_once(
    text,
    """        self.ECDH_set_method = self._lib.ECDH_set_method
        self._lib.ECDH_set_method.restype = ctypes.c_int
        self._lib.ECDH_set_method.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
""",
    """        try:
            self.ECDH_set_method = self._lib.ECDH_set_method
        except AttributeError:
            pass
        else:
            self._lib.ECDH_set_method.restype = ctypes.c_int
            self._lib.ECDH_set_method.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
""",
    label="ECDH_set_method compatibility",
)
text = replace_once(
    text,
    """        self.EVP_CIPHER_CTX_cleanup = self._lib.EVP_CIPHER_CTX_cleanup
        self.EVP_CIPHER_CTX_cleanup.restype = ctypes.c_int
        self.EVP_CIPHER_CTX_cleanup.argtypes = [ctypes.c_void_p]
""",
    """        try:
            self.EVP_CIPHER_CTX_cleanup = self._lib.EVP_CIPHER_CTX_cleanup
        except AttributeError:
            # xlog-rs compatibility patch: OpenSSL 1.1+ replaced cleanup with reset.
            self.EVP_CIPHER_CTX_cleanup = self._lib.EVP_CIPHER_CTX_reset
        self.EVP_CIPHER_CTX_cleanup.restype = ctypes.c_int
        self.EVP_CIPHER_CTX_cleanup.argtypes = [ctypes.c_void_p]
""",
    label="EVP_CIPHER_CTX_cleanup compatibility",
)
text = replace_once(
    text,
    """        self.EVP_ecdsa = self._lib.EVP_ecdsa
        self._lib.EVP_ecdsa.restype = ctypes.c_void_p
        self._lib.EVP_ecdsa.argtypes = []
""",
    """        try:
            self.EVP_ecdsa = self._lib.EVP_ecdsa
            self._lib.EVP_ecdsa.restype = ctypes.c_void_p
            self._lib.EVP_ecdsa.argtypes = []
        except AttributeError:
            # xlog-rs compatibility patch: EVP_ecdsa removed in OpenSSL 1.1+.
            self.EVP_ecdsa = self._lib.EVP_sha1
            self._lib.EVP_sha1.restype = ctypes.c_void_p
            self._lib.EVP_sha1.argtypes = []
""",
    label="EVP_ecdsa compatibility",
)
text = replace_once(
    text,
    """        self.EVP_MD_CTX_create = self._lib.EVP_MD_CTX_create
        self.EVP_MD_CTX_create.restype = ctypes.c_void_p
        self.EVP_MD_CTX_create.argtypes = []

        self.EVP_MD_CTX_init = self._lib.EVP_MD_CTX_init
        self.EVP_MD_CTX_init.restype = None
        self.EVP_MD_CTX_init.argtypes = [ctypes.c_void_p]

        self.EVP_MD_CTX_destroy = self._lib.EVP_MD_CTX_destroy
        self.EVP_MD_CTX_destroy.restype = None
        self.EVP_MD_CTX_destroy.argtypes = [ctypes.c_void_p]
""",
    """        try:
            self.EVP_MD_CTX_create = self._lib.EVP_MD_CTX_create
            self.EVP_MD_CTX_create.restype = ctypes.c_void_p
            self.EVP_MD_CTX_create.argtypes = []
        except AttributeError:
            self.EVP_MD_CTX_create = self._lib.EVP_MD_CTX_new
            self.EVP_MD_CTX_create.restype = ctypes.c_void_p
            self.EVP_MD_CTX_create.argtypes = []

        try:
            self.EVP_MD_CTX_init = self._lib.EVP_MD_CTX_init
            self.EVP_MD_CTX_init.restype = None
            self.EVP_MD_CTX_init.argtypes = [ctypes.c_void_p]
        except AttributeError:
            self.EVP_MD_CTX_init = lambda _ctx: None

        try:
            self.EVP_MD_CTX_destroy = self._lib.EVP_MD_CTX_destroy
            self.EVP_MD_CTX_destroy.restype = None
            self.EVP_MD_CTX_destroy.argtypes = [ctypes.c_void_p]
        except AttributeError:
            self.EVP_MD_CTX_destroy = self._lib.EVP_MD_CTX_free
            self.EVP_MD_CTX_destroy.restype = None
            self.EVP_MD_CTX_destroy.argtypes = [ctypes.c_void_p]
""",
    label="EVP_MD_CTX compatibility",
)
text = replace_once(
    text,
    """libname = ctypes.util.find_library('crypto')
if libname is None:
    # For Windows ...
    libname = ctypes.util.find_library('libeay32.dll')
if libname is None:
    raise Exception("Couldn't load OpenSSL lib ...")
OpenSSL = _OpenSSL(libname)
""",
    """libname = ctypes.util.find_library('crypto')
if libname is None:
    # For Windows ...
    libname = ctypes.util.find_library('libeay32.dll')
if libname is None:
    # xlog-rs compatibility patch: Homebrew OpenSSL locations.
    for candidate in (
        '/opt/homebrew/opt/openssl@1.1/lib/libcrypto.dylib',
        '/opt/homebrew/opt/openssl@3/lib/libcrypto.dylib',
        '/opt/homebrew/opt/openssl@3.5/lib/libcrypto.dylib',
        '/usr/local/opt/openssl@1.1/lib/libcrypto.dylib',
        '/usr/local/opt/openssl@3/lib/libcrypto.dylib',
    ):
        if os.path.exists(candidate):
            libname = candidate
            break
if libname is None:
    raise Exception("Couldn't load OpenSSL lib ...")
OpenSSL = _OpenSSL(libname)
""",
    label="libcrypto discovery fallback",
)
openssl_py.write_text(text)

ecc_text = ecc_py.read_text()
ecc_text = replace_once(
    ecc_text,
    "            OpenSSL.ECDH_set_method(own_key, OpenSSL.ECDH_OpenSSL())",
    "            if hasattr(OpenSSL, 'ECDH_set_method') and hasattr(OpenSSL, 'ECDH_OpenSSL'):\n                OpenSSL.ECDH_set_method(own_key, OpenSSL.ECDH_OpenSSL())",
    label="ecc ECDH method guard",
)
ecc_py.write_text(ecc_text)

for pyc_file in (site_packages / "pyelliptic").glob("*.pyc"):
    pyc_file.unlink(missing_ok=True)
PY

log "verifying decoder environment"
python - <<'PY'
import pyelliptic
import zstandard as zstd

svr = pyelliptic.ECC(curve='secp256k1')
client = pyelliptic.ECC(curve='secp256k1')
key = svr.get_ecdh_key(client.get_pubkey())
assert len(key) == 32
print("pyelliptic", getattr(pyelliptic, "__version__", "unknown"))
print("zstandard", getattr(zstd, "__version__", "unknown"))
print("ecdh_ok", len(key))
PY

py2_bin="$(pyenv prefix "$env_name")/bin/python2"
log "done"
printf '\n'
printf 'Use this Python2 decoder binary:\n'
printf '  export XLOG_PY2_BIN="%s"\n' "$py2_bin"
printf '\n'
