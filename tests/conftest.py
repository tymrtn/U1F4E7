import os
import sys
import pytest
from httpx import ASGITransport, AsyncClient

# Ensure app resolves static/templates relative to U1F4E7/
os.chdir(os.path.join(os.path.dirname(__file__), ".."))
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from app.main import app


@pytest.fixture
def client():
    transport = ASGITransport(app=app)
    return AsyncClient(transport=transport, base_url="http://test")
