# SPDX-License-Identifier: GPL-2.0-only OR MIT
obj-m := sendit_notify.o

KVER ?= $(shell uname -r)

default:
	$(MAKE) -C /lib/modules/$(KVER)/build M=$(CURDIR) modules

clean:
	$(MAKE) -C /lib/modules/$(KVER)/build M=$(CURDIR) clean
