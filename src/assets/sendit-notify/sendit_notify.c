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
 *                     root's filesystem is watched for the guest's own
 *                     changes. Only as the first record of an open file.
 *   T <ns>            the host's realtime clock, sent first in every batch
 *   c <ctime> <path>  created on the host
 *   m <ctime> <path>  content modified on the host
 *   a <ctime> <path>  metadata changed on the host
 *   r 0 <path>        removed on the host
 *   x 0 <path>        a directory removed on the host
 *
 * <ctime> is the host's change time of the path in ns, used for the echo
 * check below: unlike the mtime, it moves on with every change, also with
 * a chmod or a copy that keeps the mtime (cp -p, rsync -a).
 *
 * Paths are resolved below the root that R pinned, never through a
 * symlink or into another mount. The guest's own view decides what
 * happened: after a forced re-lookup, a path that is gone gets IN_DELETE
 * (with IN_ISDIR for x, or for a directory the guest still had cached), and
 * one that exists gets IN_CREATE (c), IN_MODIFY and IN_CLOSE_WRITE (m), or
 * IN_ATTRIB (a). For a removal, the parent comes from the guest's dentry
 * cache: the host removes a directory's contents before the directory, and
 * by the time their records arrive the directory is gone on the host too.
 *
 * The guest's own writes come back from the host as records too. A record
 * whose change time is no later than a recent local change to the same file
 * is such an echo, and is dropped so that watchers don't see the change
 * twice. The local change is noted with the guest's time when it happened.
 *
 * Each open file has its own framing, root and clock, so that a writer that
 * died in the middle of a record leaves nothing behind for the next one.
 */
#include <linux/module.h>
#include <linux/miscdevice.h>
#include <linux/fs.h>
#include <linux/dcache.h>
#include <linux/mount.h>
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
MODULE_PARM_DESC(echo_tolerance_ms, "How much later than a local change an echo's change time can be");

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

/*
 * How records are resolved below the root: never through a symlink, out of
 * the root or into another mount (such as the mask over a hidden .git).
 */
#define SN_LOOKUP (LOOKUP_BENEATH | LOOKUP_NO_SYMLINKS | LOOKUP_NO_XDEV)

/* What one open file has been sent. */
struct sn_writer {
	char buf[REC_MAX + 1]; /* the start of an unfinished record */
	size_t len;
	bool skipping; /* dropping the rest of an overlong record */
	bool started; /* past the first record, so R is refused */
	char *root; /* from R, without a trailing '/' */
	size_t root_len;
	struct path root_path; /* pinned while the file is open */
	s64 clock_offset; /* guest minus host realtime, from T */
};

/* Writes and the mark are under sn_lock. */
static DEFINE_MUTEX(sn_lock);
static struct fsnotify_mark *sn_mark;

/* The writer, whose own injected events the observer ignores. */
static struct task_struct *sn_injecting;

/* --- Echo table: recent local changes, keyed by inode or (dir, name) --- */

/* Enough for the files a guest build writes within the echo window. */
#define ECHO_BITS 16
struct echo_slot {
	u64 key;
	s64 when;
};
static struct echo_slot *echo_tab;
static DEFINE_SPINLOCK(echo_lock);

static u64 key_inode(const struct inode *inode)
{
	return ((u64)inode->i_ino << 1) | 1;
}

/*
 * A name that appeared (created, moved in) and one that vanished (deleted,
 * moved out) get separate keys: otherwise a guest create would also explain
 * a host delete of the same name, and that DELETE would be lost.
 */
static u64 key_name(const struct inode *dir, const struct qstr *name,
		    bool vanished)
{
	return ((((u64)jhash(name->name, name->len, (u32)dir->i_ino) << 32) ^
		 dir->i_ino) & ~3ULL) | (vanished ? 2 : 0);
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
 * Forgets a local change to `key`: the host has changed it since, so it
 * explains no later record.
 */
static void echo_forget(u64 key)
{
	struct echo_slot *slot = &echo_tab[hash_64(key, ECHO_BITS)];

	spin_lock(&echo_lock);
	if (slot->key == key)
		slot->key = 0;
	spin_unlock(&echo_lock);
}

/*
 * A recent local change to `key` explains a host record with this change
 * time, already moved onto the guest's clock. A collision in the table only
 * costs a duplicate event.
 */
static bool echo_hit(u64 key, s64 ctime)
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
	return ctime <= when + (s64)echo_tolerance_ms * NSEC_PER_MSEC;
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
	if (dir && name && (mask & (FS_CREATE | FS_MOVED_TO)))
		echo_note(key_name(dir, name, false));
	if (dir && name && (mask & (FS_DELETE | FS_MOVED_FROM)))
		echo_note(key_name(dir, name, true));
	/* The host changes the directory's times too. */
	if (dir && name && (mask & (FS_CREATE | FS_MOVED_TO | FS_DELETE |
				    FS_MOVED_FROM)))
		echo_note(key_inode(dir));
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

/*
 * Makes `root` the writer's project root, pinned until the file is closed,
 * and watches its filesystem.
 */
static int sn_set_root(struct sn_writer *w, const char *root)
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
	err = kern_path(root, LOOKUP_DIRECTORY | LOOKUP_NO_SYMLINKS, &path);
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
	fsnotify_init_mark(mark, sn_group);
	mark->mask = FS_MODIFY | FS_CLOSE_WRITE | FS_ATTRIB | FS_CREATE |
		     FS_DELETE | FS_MOVED_FROM | FS_MOVED_TO;
	err = fsnotify_add_mark(mark, path.dentry->d_sb, FSNOTIFY_OBJ_TYPE_SB, 0);
	if (err) {
		fsnotify_put_mark(mark);
		kfree(copy);
		path_put(&path);
		return err;
	}
	sn_mark = mark;
	w->root = copy;
	w->root_len = len;
	w->root_path = path;
	return 0;
}

/* --- Injection --- */

static bool ends_with(const char *s, size_t len, const char *suffix)
{
	size_t n = strlen(suffix);

	return len >= n && !memcmp(s + len - n, suffix, n);
}

/*
 * `path` lies below the root as text, with no empty, "." or ".." component.
 * The lookups enforce it; this is a first filter.
 */
static bool sn_path_ok(const struct sn_writer *w, const char *path)
{
	size_t len = strlen(path);

	if (!w->root || len <= w->root_len + 1 ||
	    memcmp(path, w->root, w->root_len) || path[w->root_len] != '/')
		return false;
	return !strstr(path, "/../") && !strstr(path, "/./") &&
	       !strstr(path, "//") && !ends_with(path, len, "/..") &&
	       !ends_with(path, len, "/.") && !ends_with(path, len, "/");
}

/* The guest finds `name` in `dir` gone. */
static void sn_gone(s64 ctime, struct inode *dir, const struct qstr *name,
		    bool is_dir, const char *pathname)
{
	if (echo_hit(key_name(dir, name, true), ctime)) {
		n_echo++;
		return;
	}
	fsnotify_name(FS_DELETE | (is_dir ? FS_ISDIR : 0), dir,
		      FSNOTIFY_EVENT_INODE, dir, name, 0);
	/* A guest create from before doesn't explain the host's next one. */
	echo_forget(key_name(dir, name, false));
	n_injected++;
	if (verbose)
		pr_info("sendit-notify: DELETE%s %s\n", is_dir ? ",ISDIR" : "",
			pathname);
}

/* The guest finds `name` in `dir` present, at `child`. */
static void sn_present(char op, s64 ctime, struct inode *dir,
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

	if (echo_hit(key_inode(inode), ctime)) {
		n_echo++;
		return;
	}
	switch (op) {
	case 'c':
		fsnotify_name(FS_CREATE | (is_dir ? FS_ISDIR : 0), dentry,
			      FSNOTIFY_EVENT_DENTRY, dir, name, 0);
		/* A guest delete from before doesn't explain the host's next one. */
		echo_forget(key_name(dir, name, true));
		if (!is_dir) {
			fsnotify_dentry(dentry, FS_MODIFY);
			fsnotify_dentry(dentry, FS_CLOSE_WRITE);
		}
		break;
	case 'a':
		fsnotify_dentry(dentry, FS_ATTRIB);
		break;
	default: /* 'm', or 'r' and 'x' for a path that is back again */
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

/*
 * The directory `rel` (below the root) of a removed path, as the guest
 * still has it in its dentry cache. Not looked up: virtiofs would ask the
 * host, which has removed it with its contents by now, and then drop the
 * guest's entry too. Only directories are walked through, no symlink and
 * no other mount.
 */
static int sn_removal_parent(const struct sn_writer *w, char *rel,
			     struct path *parent)
{
	struct dentry *dentry = dget(w->root_path.dentry);
	char *p = rel;

	while (*p) {
		char *end = strchrnul(p, '/');
		struct qstr name = QSTR_INIT(p, end - p);
		struct dentry *next = d_hash_and_lookup(dentry, &name);

		dput(dentry);
		if (IS_ERR_OR_NULL(next))
			return next ? PTR_ERR(next) : -ENOENT;
		if (!d_really_is_positive(next) || !d_is_dir(next) ||
		    d_mountpoint(next)) {
			dput(next);
			return -ENOENT;
		}
		dentry = next;
		p = *end ? end + 1 : end;
	}
	parent->mnt = mntget(w->root_path.mnt);
	parent->dentry = dentry;
	return 0;
}

/* `pathname` passed sn_path_ok. */
static void sn_inject(struct sn_writer *w, char op, s64 ctime, char *pathname)
{
	char *rel = pathname + w->root_len + 1;
	char *slash = strrchr(rel, '/');
	const char *last = slash ? slash + 1 : rel;
	struct qstr name = QSTR_INIT(last, strlen(last));
	bool removal = op == 'r' || op == 'x';
	bool is_dir = op == 'x';
	struct path parent, child;
	struct dentry *cached;
	struct inode *dir;
	int err;

	if (slash) {
		*slash = '\0';
		/*
		 * Without LOOKUP_REVAL: a parent the guest has cached is
		 * enough, and for a removal it is likely gone on the host.
		 */
		err = removal ? sn_removal_parent(w, rel, &parent) :
				vfs_path_lookup(w->root_path.dentry,
						w->root_path.mnt, rel,
						SN_LOOKUP | LOOKUP_DIRECTORY,
						&parent);
		*slash = '/';
		if (err) {
			/* The parent is gone too: its own record reports that. */
			if (verbose)
				pr_info("sendit-notify: no parent for %s: %d\n",
					pathname, err);
			return;
		}
	} else {
		parent = w->root_path;
		path_get(&parent);
	}
	dir = d_inode(parent.dentry);

	/*
	 * Echoes are dropped before the forced re-lookup, which costs round
	 * trips to the host, whenever the guest's cache can tell.
	 */
	if (op == 'c' && echo_hit(key_name(dir, &name, false), ctime))
		goto echo;
	cached = d_hash_and_lookup(parent.dentry, &name);
	if (!IS_ERR_OR_NULL(cached)) {
		bool hit = false;

		if (d_really_is_positive(cached)) {
			is_dir |= d_is_dir(cached);
			hit = !removal &&
			      echo_hit(key_inode(d_inode(cached)), ctime);
		}
		dput(cached);
		if (hit)
			goto echo;
	}

	/* LOOKUP_REVAL makes virtiofs drop cached entries the host removed. */
	err = vfs_path_lookup(parent.dentry, parent.mnt, last,
			      SN_LOOKUP | LOOKUP_REVAL, &child);
	if (err == -ENOENT) {
		sn_gone(ctime, dir, &name, is_dir, pathname);
	} else if (!err) {
		sn_present(op, ctime, dir, &name, &child, pathname);
		path_put(&child);
	} else {
		n_errors++;
	}
	path_put(&parent);
	return;
echo:
	n_echo++;
	path_put(&parent);
}

/* Handles one record of `len` bytes, without its '\0'. */
static void sn_record(struct sn_writer *w, char *rec, size_t len)
{
	bool first = !w->started;
	char op = rec[0];
	char *path;
	s64 val;
	int err;

	if (!len)
		return;
	w->started = true;
	if (len < 3 || rec[1] != ' ')
		goto bad;
	if (op == 'T') {
		if (kstrtos64(rec + 2, 10, &val))
			goto bad;
		w->clock_offset = (s64)ktime_get_real_ns() - val;
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
		/* Only the service sends it, before anything else. */
		if (!first)
			goto bad;
		err = sn_set_root(w, path);
		if (err) {
			pr_warn("sendit-notify: can't watch %s: %d\n", path, err);
			goto bad;
		}
		pr_info("sendit-notify: watching %s for local changes\n", path);
		return;
	case 'c':
	case 'm':
	case 'a':
		val += w->clock_offset;
		fallthrough;
	case 'r':
	case 'x':
		if (!sn_path_ok(w, path))
			goto bad;
		sn_inject(w, op, val, path);
		return;
	}
bad:
	n_errors++;
}

static ssize_t sn_write(struct file *file, const char __user *ubuf,
			size_t len, loff_t *off)
{
	struct sn_writer *w = file->private_data;
	size_t done = 0;
	int err = 0;

	if (mutex_lock_killable(&sn_lock))
		return -EINTR;
	WRITE_ONCE(sn_injecting, current);
	while (done < len) {
		size_t chunk = min(len - done, REC_MAX + 1 - w->len);
		size_t start = 0, i;

		if (copy_from_user(w->buf + w->len, ubuf + done, chunk)) {
			err = -EFAULT;
			break;
		}
		done += chunk;
		for (i = w->len; i < w->len + chunk; i++) {
			if (w->buf[i])
				continue;
			if (w->skipping)
				w->skipping = false;
			else
				sn_record(w, w->buf + start, i - start);
			start = i + 1;
		}
		w->len += chunk;
		if (start == 0 && w->len == REC_MAX + 1) {
			/* Longer than any record: drop it up to its '\0'. */
			if (!w->skipping)
				n_errors++;
			w->skipping = true;
			w->len = 0;
			continue;
		}
		memmove(w->buf, w->buf + start, w->len - start);
		w->len -= start;
	}
	WRITE_ONCE(sn_injecting, NULL);
	mutex_unlock(&sn_lock);
	return done ? (ssize_t)done : err;
}

static int sn_open(struct inode *inode, struct file *file)
{
	struct sn_writer *w = kzalloc(sizeof(*w), GFP_KERNEL);

	if (!w)
		return -ENOMEM;
	file->private_data = w;
	return 0;
}

/*
 * Unpins the root, so the project's mount isn't kept busy once the service
 * has stopped, e.g. at shutdown. The mark stays until the next R or unload.
 */
static int sn_release(struct inode *inode, struct file *file)
{
	struct sn_writer *w = file->private_data;

	if (w->root) {
		path_put(&w->root_path);
		kfree(w->root);
	}
	kfree(w);
	return 0;
}

static const struct file_operations sn_fops = {
	.owner = THIS_MODULE,
	.open = sn_open,
	.release = sn_release,
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

	echo_tab = kvcalloc(1 << ECHO_BITS, sizeof(*echo_tab), GFP_KERNEL);
	if (!echo_tab)
		return -ENOMEM;
	sn_group = fsnotify_alloc_group(&sn_ops, 0);
	if (IS_ERR(sn_group)) {
		kvfree(echo_tab);
		return PTR_ERR(sn_group);
	}
	err = misc_register(&sn_dev);
	if (err) {
		fsnotify_put_group(sn_group);
		kvfree(echo_tab);
		return err;
	}
	pr_info("sendit-notify: loaded\n");
	return 0;
}

/*
 * An open device holds a module reference, so no write runs here, and every
 * root is unpinned. The group can't be destroyed directly
 * (fsnotify_destroy_group isn't exported): once its mark is freed, the last
 * put frees the group.
 */
static void __exit sn_exit(void)
{
	misc_deregister(&sn_dev);
	sn_unwatch();
	fsnotify_wait_marks_destroyed();
	fsnotify_put_group(sn_group);
	kvfree(echo_tab);
}

module_init(sn_init);
module_exit(sn_exit);
MODULE_LICENSE("Dual MIT/GPL");
MODULE_DESCRIPTION("Emits fsnotify events for changes made on the sendit host");
