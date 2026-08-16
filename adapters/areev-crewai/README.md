# areev-crewai

Areev storage backend for CrewAI memory: every consolidation rewrite is
a supersession chain, every delete an audited tombstone, and `FORGET
SUBJECT` over a memory source erases its records with a receipt.

```python
from areev_crewai import AreevStorageBackend
backend = AreevStorageBackend("./crew-memory.db")
```
