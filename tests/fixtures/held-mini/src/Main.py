"""Python fixture mirroring Main.kt — exercises the import/alias/decorator/base-class cases that a
naive Python edge extractor gets wrong."""

from .sessions import Session as ClientSession
from requests.adapters import HTTPAdapter
import urllib3 as http

DEFAULT_TIMEOUT = 30
default_retries = 3


class Api(Session):
    """A small client built on the imported Session base class."""

    @classmethod
    def from_url(cls, url: str) -> "Api":
        LOCAL_MAX = 5
        adapter = HTTPAdapter()
        http.disable_warnings()
        return cls()

    @property
    def host(self) -> str:
        return "example.com"


def make():
    return ClientSession()
