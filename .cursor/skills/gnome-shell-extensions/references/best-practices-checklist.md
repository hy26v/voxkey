# Extension best-practices checklist

Use before finishing Shell extension changes:

- [ ] Nothing constructed before `enable()`
- [ ] `disable()` destroys actors, removes sources, disconnects, nulls refs
- [ ] `enable()` and `disable()` sit next to each other
- [ ] No blanket try/catch around destroy/disconnect/Source.remove
- [ ] No optional chaining on guaranteed Shell APIs for the targeted version
- [ ] Timeouts: clear previous id immediately before scheduling a new one
- [ ] Icons are symbolic names, not emoji
- [ ] Heavy work stays in the daemon; extension uses D-Bus
- [ ] No leftover placeholder stubs
- [ ] Shell version / upgrade notes checked if touching Shell UI APIs
- [ ] Wayland reload plan: logout/login (VM doc), not settings restart alone
