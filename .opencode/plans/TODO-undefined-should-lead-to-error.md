Trying to access undefined variables *should absolutely* lead to application error.

This includes trying to access unknown properties inside of TOML/JSON, or even an undefined environment variable.

This needs to be tested thoroughly.
