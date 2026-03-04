import os
import base64
import hashlib
from cryptography.fernet import Fernet


def _get_fernet() -> Fernet:
    key = os.getenv("ENVELOPE_SECRET_KEY")
    if not key:
        raise RuntimeError("ENVELOPE_SECRET_KEY environment variable is required")
    # If the key isn't valid Fernet format (44 url-safe base64 chars),
    # derive a valid key from it via SHA-256
    try:
        Fernet(key.encode() if isinstance(key, str) else key)
        fernet_key = key.encode() if isinstance(key, str) else key
    except Exception:
        derived = hashlib.sha256(key.encode()).digest()
        fernet_key = base64.urlsafe_b64encode(derived)
    return Fernet(fernet_key)


def encrypt(plaintext: str) -> str:
    f = _get_fernet()
    return f.encrypt(plaintext.encode()).decode()


def decrypt(ciphertext: str) -> str:
    f = _get_fernet()
    return f.decrypt(ciphertext.encode()).decode()
