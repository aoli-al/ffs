# Builds every tests/c/*.c into tests/bin/<name>.  Run with `make -C tests/c`.
CC      ?= gcc
CFLAGS  ?= -O1 -g -pthread -I../../include -fno-omit-frame-pointer -no-pie -fno-pie -D_GNU_SOURCE
BINDIR  := ../bin
SRCS    := $(wildcard *.c)
BINS    := $(patsubst %.c,$(BINDIR)/%,$(SRCS))

.PHONY: all clean
all: $(BINS)

$(BINDIR)/%: %.c ../../include/ffs.h | $(BINDIR)
	$(CC) $(CFLAGS) -o $@ $<

$(BINDIR):
	mkdir -p $(BINDIR)

clean:
	rm -f $(BINS)
