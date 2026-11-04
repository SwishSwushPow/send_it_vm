// SPDX-License-Identifier: GPL-2.0-only OR MIT
/*
 * sendit_notify: emits fsnotify events for changes made on the host.
 *
 * virtiofs gives the guest no inotify events for edits made on the Mac, so
 * `sendit run` sends records that describe them, and a root service writes
 * them to /dev/sendit-notify. Each record ends with '\0', since a path may
 * contain '\n', and its path is raw bytes:
 *
 *   R 0 <root>        the project root; paths must lie below it, and the
 *                     root's filesystem is watched for the guest's own changes
 *   T <ns>            the host's realtime clock, sent first in every batch
 *   c <mtime> <path>  created on the host
 *   m <mtime> <path>  content modified on the host
 *   a <mtime> <path>  metadata changed on the host
 *   r 0 <path>        removed on the host
 *
 * The guest's own view decides what happened: after a forced re-lookup, a
 * path that is gone gets IN_DELETE, and one that exists gets IN_CREATE (c),
 * IN_MODIFY and IN_CLOSE_WRITE (m), or IN_ATTRIB (a).
 *
 * The guest's own writes come back from the host as records too. A record
 * whose mtime is no later than a recent local change to the same file is
 * such an echo, and is dropped so that watchers don't see the change twice.
 */
#include <linux/module.h>
#include <linux/miscdevice.h>
#include <linux/fs.h>
#include <linux/namei.h>
#include <linux/fsnotify.h>
#include <linux/fsnotify_backend.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/uaccess.h>
#include <linux/mutex.h>
#include <linux/spinlock.h>
#include <linux/stat.h>
#include <linux/timekeeping.h>
#include <linux/hash.h>
#include <linux/jhash.h>

static int echo_window_ms = 5000;
module_param(echo_window_ms, int, 0644);
MODULE_PARM_DESC(echo_window_ms, "How long a local change can explain a host record");

static int echo_tolerance_ms = 100;
module_param(echo_tolerance_ms, int, 0644);
MODULE_PARM_DESC(echo_tolerance_ms, "How much later than a local change an echo's mtime can be");

static bool verbose;
module_param(verbose, bool, 0644);
MODULE_PARM_DESC(verbose, "Log every injected event");

static unsigned long n_injected, n_echo, n_errors;
module_param(n_injected, ulong, 0444);
MODULE_PARM_DESC(n_injected, "Records turned into events");
module_param(n_echo, ulong, 0444);
MODULE_PARM_DESC(n_echo, "Records dropped as echoes of the guest's own changes");
module_param(n_errors, ulong, 0444);
MODULE_PARM_DESC(n_errors, "Records that were malformed or outside the root");

/* The longest record; longer ones are dropped. */
#define REC_MAX (PATH_MAX + 64)

/* Everything the writer uses is under sn_lock. */
static DEFINE_MUTEX(sn_lock);
static char *sn_buf; /* REC_MAX + 1 bytes: the start of an unfinished record */
static size_t sn_len;
static bool sn_skipping; /* dropping the rest of an overlong record */
static char *sn_root; /* from R, without a trailing '/' */
static size_t sn_root_len;
static s64 sn_clock_offset; /* guest minus host realtime, from T */
static struct fsnotify_mark *sn_mark;

/* The writer, whose own injected events the observer ignores. */
static struct task_struct *sn_injecting;

/* --- Echo table: recent local changes, keyed by inode or (dir, name) --- */

#define ECHO_BITS 12
struct echo_slot {
	u64 key;
	s64 when;
};
static struct echo_slot echo_tab[1 << ECHO_BITS];
static DEFINE_SPINLOCK(echo_lock);

static u64 key_inode(const struct inode *inode)
{
	return ((u64)inode->i_ino << 1) | 1;
}

static u64 key_name(const struct inode *dir, const struct qstr *name)
{
	return (((u64)jhash(name->name, name->len, (u32)dir->i_ino) << 32) ^
		dir->i_ino) & ~1ULL;
}

static void echo_note(u64 key)
{
	struct echo_slot *slot = &echo_tab[hash_64(key, ECHO_BITS)];

	spin_lock(&echo_lock);
	slot->key = key;
	slot->when = ktime_get_real_ns();
	spin_unlock(&echo_lock);
}

/*
 * A recent local change to `key` explains a host record with this mtime,
 * already moved onto the guest's clock. A collision in the table only costs
 * a duplicate event.
 */
static bool echo_hit(u64 key, s64 mtime)
{
	struct echo_slot *slot = &echo_tab[hash_64(key, ECHO_BITS)];
	s64 now = ktime_get_real_ns(), when;
	bool hit;

	spin_lock(&echo_lock);
	hit = slot->key == key;
	when = slot->when;
	spin_unlock(&echo_lock);
	if (!hit || now - when > (s64)echo_window_ms * NSEC_PER_MSEC)
		return false;
	return mtime <= when + (s64)echo_tolerance_ms * NSEC_PER_MSEC;
}

/* --- Observer of local changes on the project's filesystem --- */

static struct fsnotify_group *sn_group;

/*
 * handle_event rather than handle_inode_event: the generic handler that
 * calls the latter WARNs on superblock marks.
 */
static int sn_handle_event(struct fsnotify_group *group, u32 mask,
			   const void *data, int data_type, struct inode *dir,
			   const struct qstr *name, u32 cookie,
			   struct fsnotify_iter_info *iter_info)
{
	struct inode *inode;

	if (current == READ_ONCE(sn_injecting))
		return 0;
	inode = fsnotify_data_inode(data, data_type);
	if (inode && (mask & (FS_MODIFY | FS_CLOSE_WRITE | FS_ATTRIB |
			      FS_CREATE | FS_MOVED_TO)))
		echo_note(key_inode(inode));
	if (dir && name && (mask & (FS_CREATE | FS_DELETE | FS_MOVED_FROM |
				    FS_MOVED_TO)))
		echo_note(key_name(dir, name));
	return 0;
}

static void sn_free_mark(struct fsnotify_mark *mark)
{
	kfree(mark);
}

static const struct fsnotify_ops sn_ops = {
	.handle_event = sn_handle_event,
	.free_mark = sn_free_mark,
};

static void sn_unwatch(void)
{
	if (sn_mark) {
		fsnotify_destroy_mark(sn_mark, sn_group);
		fsnotify_put_mark(sn_mark);
		sn_mark = NULL;
	}
}

/* Makes `root` the project root and watches its filesystem. */
static int sn_set_root(const char *root)
{
	size_t len = strlen(root);
	struct fsnotify_mark *mark;
	struct path path;
	char *copy;
	int err;

	while (len && root[len - 1] == '/')
		len--;
	if (root[0] != '/' || !len)
		return -EINVAL;
	err = kern_path(root, LOOKUP_FOLLOW | LOOKUP_DIRECTORY, &path);
	if (err)
		return err;
	copy = kmemdup_nul(root, len, GFP_KERNEL);
	mark = kzalloc(sizeof(*mark), GFP_KERNEL);
	if (!copy || !mark) {
		kfree(mark);
		kfree(copy);
		path_put(&path);
		return -ENOMEM;
	}

	/* A group can only have one mark on a superblock. */
	sn_unwatch();
	kfree(sn_root);
	sn_root = NULL;

	fsnotify_init_mark(mark, sn_group);
	mark->mask = FS_MODIFY | FS_CLOSE_WRITE | FS_ATTRIB | FS_CREATE |
		     FS_DELETE | FS_MOVED_FROM | FS_MOVED_TO;
	err = fsnotify_add_mark(mark, path.dentry->d_sb, FSNOTIFY_OBJ_TYPE_SB, 0);
	path_put(&path);
	if (err) {
		fsnotify_put_mark(mark);
		kfree(copy);
		return err;
	}
	sn_mark = mark;
	sn_root = copy;
	sn_root_len = len;
	return 0;
}

/* --- Injection --- */

static bool ends_with(const char *s, size_t len, const char *suffix)
{
	size_t n = strlen(suffix);

	return len >= n && !memcmp(s + len - n, suffix, n);
}

/* `path` lies below the root, with no "." or ".." component to leave it. */
static bool sn_path_ok(const char *path)
{
	size_t len = strlen(path);

	if (!sn_root || len <= sn_root_len + 1 ||
	    memcmp(path, sn_root, sn_root_len) || path[sn_root_len] != '/')
		return false;
	return !strstr(path, "/../") && !strstr(path, "/./") &&
	       !ends_with(path, len, "/..") && !ends_with(path, len, "/.") &&
	       !ends_with(path, len, "/");
}

/* The guest finds `name` in `dir` gone. */
static void sn_gone(s64 mtime, struct inode *dir, const struct qstr *name,
		    const char *pathname)
{
	if (echo_hit(key_name(dir, name), mtime)) {
		n_echo++;
		return;
	}
	fsnotify_name(FS_DELETE, dir, FSNOTIFY_EVENT_INODE, dir, name, 0);
	n_injected++;
	if (verbose)
		pr_info("sendit-notify: DELETE %s\n", pathname);
}

/* The guest finds `name` in `dir` present, at `child`. */
static void sn_present(char op, s64 mtime, struct inode *dir,
		       const struct qstr *name, const struct path *child,
		       const char *pathname)
{
	struct dentry *dentry = child->dentry;
	struct inode *inode = d_inode(dentry);
	bool is_dir = S_ISDIR(inode->i_mode);
	struct kstat stat;

	/*
	 * A cheap safeguard: fetch fresh attributes, which also drops cached
	 * pages if the size or mtime changed. The events go out either way.
	 */
	vfs_getattr(child, &stat, STATX_BASIC_STATS, AT_STATX_FORCE_SYNC);

	if (echo_hit(key_inode(inode), mtime) ||
	    (op == 'c' && echo_hit(key_name(dir, name), mtime))) {
		n_echo++;
		return;
	}
	switch (op) {
	case 'c':
		fsnotify_name(FS_CREATE | (is_dir ? FS_ISDIR : 0), dentry,
			      FSNOTIFY_EVENT_DENTRY, dir, name, 0);
		if (!is_dir) {
			fsnotify_dentry(dentry, FS_MODIFY);
			fsnotify_dentry(dentry, FS_CLOSE_WRITE);
		}
		break;
	case 'a':
		fsnotify_dentry(dentry, FS_ATTRIB);
		break;
	default: /* 'm', or 'r' for a path that is back again */
		if (is_dir)
			return;
		fsnotify_dentry(dentry, FS_MODIFY);
		fsnotify_dentry(dentry, FS_CLOSE_WRITE);
		break;
	}
	n_injected++;
	if (verbose)
		pr_info("sendit-notify: %c %s\n", op, pathname);
}

/* `pathname` passed sn_path_ok, so it has a parent below the root. */
static void sn_inject(char op, s64 mtime, char *pathname)
{
	char *slash = strrchr(pathname, '/');
	struct path parent, child;
	struct qstr name;
	struct inode *dir;
	int err;

	*slash = '\0';
	err = kern_path(pathname, LOOKUP_FOLLOW | LOOKUP_DIRECTORY, &parent);
	*slash = '/';
	if (err) {
		/* The parent is gone too: its own record reports that. */
		if (verbose)
			pr_info("sendit-notify: no parent for %s: %d\n",
				pathname, err);
		return;
	}
	dir = d_inode(parent.dentry);
	name = (struct qstr)QSTR_INIT(slash + 1, strlen(slash + 1));

	/* LOOKUP_REVAL makes virtiofs drop cached entries the host removed. */
	err = kern_path(pathname, LOOKUP_REVAL, &child);
	if (err == -ENOENT) {
		sn_gone(mtime, dir, &name, pathname);
	} else if (!err) {
		sn_present(op, mtime, dir, &name, &child, pathname);
		path_put(&child);
	} else {
		n_errors++;
	}
	path_put(&parent);
}

/* Handles one record of `len` bytes, without its '\0'. */
static void sn_record(char *rec, size_t len)
{
	char op = rec[0];
	char *path;
	s64 val;
	int err;

	if (!len)
		return;
	if (len < 3 || rec[1] != ' ')
		goto bad;
	if (op == 'T') {
		if (kstrtos64(rec + 2, 10, &val))
			goto bad;
		sn_clock_offset = (s64)ktime_get_real_ns() - val;
		return;
	}
	path = strchr(rec + 2, ' ');
	if (!path)
		goto bad;
	*path++ = '\0';
	if (kstrtos64(rec + 2, 10, &val))
		goto bad;

	switch (op) {
	case 'R':
		err = sn_set_root(path);
		if (err) {
			pr_warn("sendit-notify: can't watch %s: %d\n", path, err);
			goto bad;
		}
		pr_info("sendit-notify: watching %s for local changes\n", path);
		return;
	case 'c':
	case 'm':
	case 'a':
		val += sn_clock_offset;
		fallthrough;
	case 'r':
		if (!sn_path_ok(path))
			goto bad;
		sn_inject(op, val, path);
		return;
	}
bad:
	n_errors++;
}

static ssize_t sn_write(struct file *file, const char __user *ubuf,
			size_t len, loff_t *off)
{
	size_t done = 0;
	int err = 0;

	if (mutex_lock_killable(&sn_lock))
		return -EINTR;
	WRITE_ONCE(sn_injecting, current);
	while (done < len) {
		size_t chunk = min(len - done, REC_MAX + 1 - sn_len);
		size_t start = 0, i;

		if (copy_from_user(sn_buf + sn_len, ubuf + done, chunk)) {
			err = -EFAULT;
			break;
		}
		done += chunk;
		for (i = sn_len; i < sn_len + chunk; i++) {
			if (sn_buf[i])
				continue;
			if (sn_skipping)
				sn_skipping = false;
			else
				sn_record(sn_buf + start, i - start);
			start = i + 1;
		}
		sn_len += chunk;
		if (start == 0 && sn_len == REC_MAX + 1) {
			/* Longer than any record: drop it up to its '\0'. */
			if (!sn_skipping)
				n_errors++;
			sn_skipping = true;
			sn_len = 0;
			continue;
		}
		memmove(sn_buf, sn_buf + start, sn_len - start);
		sn_len -= start;
	}
	WRITE_ONCE(sn_injecting, NULL);
	mutex_unlock(&sn_lock);
	return done ? (ssize_t)done : err;
}

static const struct file_operations sn_fops = {
	.owner = THIS_MODULE,
	.write = sn_write,
	.llseek = noop_llseek,
};

static struct miscdevice sn_dev = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "sendit-notify",
	.fops = &sn_fops,
	.mode = 0600,
};

static int __init sn_init(void)
{
	int err;

	sn_buf = kmalloc(REC_MAX + 1, GFP_KERNEL);
	if (!sn_buf)
		return -ENOMEM;
	sn_group = fsnotify_alloc_group(&sn_ops, 0);
	if (IS_ERR(sn_group)) {
		kfree(sn_buf);
		return PTR_ERR(sn_group);
	}
	err = misc_register(&sn_dev);
	if (err) {
		fsnotify_put_group(sn_group);
		kfree(sn_buf);
		return err;
	}
	pr_info("sendit-notify: loaded\n");
	return 0;
}

/*
 * The open device holds a module reference, so no write runs here. The
 * group can't be destroyed directly (fsnotify_destroy_group isn't
 * exported): once its mark is freed, the last put frees the group.
 */
static void __exit sn_exit(void)
{
	misc_deregister(&sn_dev);
	sn_unwatch();
	fsnotify_wait_marks_destroyed();
	fsnotify_put_group(sn_group);
	kfree(sn_root);
	kfree(sn_buf);
}

module_init(sn_init);
module_exit(sn_exit);
MODULE_LICENSE("Dual MIT/GPL");
MODULE_DESCRIPTION("Emits fsnotify events for changes made on the sendit host");
