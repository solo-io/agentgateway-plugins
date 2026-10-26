CC ?= cc
CFLAGS ?= -O2 -Wall -Wextra

plugin.so: hello.c ../../agentgateway_abi.h
	$(CC) $(CFLAGS) -I../.. -shared -fPIC -fvisibility=hidden -o $@ hello.c

.PHONY: clean
clean:
	rm -f plugin.so
