CARGO ?= cargo

.PHONY: all test clean

all:
	$(CARGO) build --release --locked

test:
	$(CARGO) test --locked

clean:
	$(CARGO) clean
