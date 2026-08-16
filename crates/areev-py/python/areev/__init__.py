"""Areev — embedded memory engine for AI agents.

Re-exports the native extension module (`areev.areev`). Convenience
functions for scripts and notebooks live in `areev.helpers` (imported
explicitly, never eagerly — the core surface stays exactly the native class).
"""
from .areev import *  # noqa: F401,F403
from . import areev as _native

__doc__ = _native.__doc__ or __doc__
# A pyo3 module defines no `__all__`, so name the public surface here rather
# than letting `from areev import *` re-export whatever the extension happens
# to carry.
__all__ = list(getattr(_native, "__all__", None)
               or [n for n in dir(_native) if not n.startswith("_")])
