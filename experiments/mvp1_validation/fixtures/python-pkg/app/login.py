# Fixture file for the HORO-1127 release-gate corpus. Exists only so
# bounded reconnaissance has a real "login"-named path to match against
# prompts like "add input validation to the login handler".


def login(_username: str, _password: str) -> bool:
    return False
