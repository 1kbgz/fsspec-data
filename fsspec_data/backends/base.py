from fsspec import AbstractFileSystem
from fsspec.spec import AbstractBufferedFile


class BaseFileSystem(AbstractFileSystem):
    """
    Docs: https://filesystem-spec.readthedocs.io/en/latest/developer.html#implementing-a-backend
    Minimal: https://github.com/fsspec/filesystem_spec/blob/45dcfa99e2dc320bf072c28bef767b71f8299a4a/fsspec/implementations/github.py#L10
    """

    # def start_transaction(self):
    # def end_transaction(self):
    # def invalidate_cache(self, path=None):

    def mkdir(self, path, create_parents=True, **kwargs):
        """Create directory entry at path

        For systems that don't have true directories, may create an for
        this instance only and not touch the real filesystem

        Parameters
        ----------
        path: str
            location
        create_parents: bool
            if True, this is equivalent to ``makedirs``
        kwargs:
            may be permissions, etc.
        """
        ...

    def makedirs(self, path, exist_ok=False):
        """Recursively make directories

        Creates directory at path and any intervening required directories.
        Raises exception if, for instance, the path already exists but is a
        file.

        Parameters
        ----------
        path: str
            leaf directory name
        exist_ok: bool (False)
            If False, will error if the target already exists
        """
        ...

    def rmdir(self, path):
        """Remove a directory, if empty"""
        ...

    def ls(self, path, detail=True, **kwargs):
        """List objects at path.

        This should include subdirectories and files at that location. The
        difference between a file and a directory must be clear when details
        are requested.

        The specific keys, or perhaps a FileInfo class, or similar, is TBD,
        but must be consistent across implementations.
        Must include:

        - full path to the entry (without protocol)
        - size of the entry, in bytes. If the value cannot be determined, will
          be ``None``.
        - type of entry, "file", "directory" or other

        Additional information
        may be present, aproriate to the file-system, e.g., generation,
        checksum, etc.

        May use refresh=True|False to allow use of self._ls_from_cache to
        check for a saved listing and avoid calling the backend. This would be
        common where listing may be expensive.

        Parameters
        ----------
        path: str
        detail: bool
            if True, gives a list of dictionaries, where each is the same as
            the result of ``info(path)``. If False, gives a list of paths
            (str).
        kwargs: may have additional backend-specific options, such as version
            information

        Returns
        -------
        List of strings if detail is False, or list of directory information
        dicts if detail is True.
        """
        raise NotImplementedError

    def cp_file(self, path1, path2, **kwargs):
        raise NotImplementedError

    def _rm(self, path):
        """Delete one file"""
        # this is the old name for the method, prefer rm_file
        raise NotImplementedError

    def created(self, path):
        """Return the created timestamp of a file as a datetime.datetime"""
        raise NotImplementedError

    def modified(self, path):
        """Return the modified timestamp of a file as a datetime.datetime"""
        raise NotImplementedError

    # def walk(self, path, maxdepth=None, **kwargs):
    # def find(self, path, maxdepth=None, withdirs=False, **kwargs):
    # def du(self, path, total=True, maxdepth=None, **kwargs):
    # def glob(self, path, **kwargs):
    # def exists(self, path, **kwargs):
    # def lexists(self, path, **kwargs):
    # def info(self, path, **kwargs):
    # def checksum(self, path):
    # def size(self, path):
    # def sizes(self, paths):
    # def isdir(self, path):
    # def isfile(self, path):
    # def cat_file(self, path, start=None, end=None, **kwargs):
    # def pipe_file(self, path, value, **kwargs):
    # def pipe(self, path, value=None, **kwargs):
    # def cat_ranges(self, paths, starts, ends, max_gap=None, **kwargs):
    # def cat(self, path, recursive=False, on_error="raise", **kwargs):
    # def get_file(self, rpath, lpath, callback=_DEFAULT_CALLBACK, **kwargs):
    # def get(self, rpath, lpath, recursive=False, callback=_DEFAULT_CALLBACK, **kwargs):
    # def put_file(self, lpath, rpath, callback=_DEFAULT_CALLBACK, **kwargs):
    # def put(self, lpath, rpath, recursive=False, callback=_DEFAULT_CALLBACK, **kwargs):
    # def head(self, path, size=1024):
    # def tail(self, path, size=1024):
    # def copy(self, path1, path2, recursive=False, on_error=None, **kwargs):
    # def expand_path(self, path, recursive=False, maxdepth=None):
    # def mv(self, path1, path2, recursive=False, maxdepth=None, **kwargs):
    # def rm_file(self, path):
    # def rm(self, path, recursive=False, maxdepth=None):
    # def _parent(cls, path):
    # def _open(
    # def open(
    # def touch(self, path, truncate=True, **kwargs):
    # def ukey(self, path):
    # def read_block(self, fn, offset, length, delimiter=None):
    # def to_json(self):
    # def from_json(blob):
    # def get_mapper(self, root="", check=False, create=False, missing_exceptions=None):
    # ------------------------------------------------------------------------
    # Aliases
    # def makedir(self, path, create_parents=True, **kwargs):
    # def mkdirs(self, path, exist_ok=False):
    # def listdir(self, path, detail=True, **kwargs):
    # def cp(self, path1, path2, **kwargs):
    # def move(self, path1, path2, **kwargs):
    # def stat(self, path, **kwargs):
    # def disk_usage(self, path, total=True, maxdepth=None, **kwargs):
    # def rename(self, path1, path2, **kwargs):
    # def delete(self, path, recursive=False, maxdepth=None):
    # def upload(self, lpath, rpath, recursive=False, **kwargs):
    # def download(self, rpath, lpath, recursive=False, **kwargs):

    def sign(self, path, expiration=100, **kwargs):
        """Create a signed URL representing the given path

        Some implementations allow temporary URLs to be generated, as a
        way of delegating credentials.

        Parameters
        ----------
        path : str
             The path on the filesystem
        expiration : int
            Number of seconds to enable the URL for (if supported)

        Returns
        -------
        URL : str
            The signed URL

        Raises
        ------
        NotImplementedError : if method is not implemented for a filesystem
        """
        raise NotImplementedError("Sign is not implemented for this filesystem")


class BaseBufferedFile(AbstractBufferedFile):
    # def details(self):
    # def details(self, value):
    # def full_name(self):
    # def closed(self):
    # def closed(self, c):

    def commit(self):
        """Move from temp to final destination"""

    def discard(self):
        """Throw away temporary file"""

    # def info(self):
    # def tell(self):
    # def seek(self, loc, whence=0):
    # def write(self, data):
    # def flush(self, force=False):
    # def _upload_chunk(self, final=False):

    def _initiate_upload(self):
        """Create remote file/upload"""
        pass

    def _fetch_range(self, start, end):
        """Get the specified set of bytes from remote"""
        raise NotImplementedError

    # def read(self, length=-1):
    # def readinto(self, b):
    # def readuntil(self, char=b"\n", blocks=None):
    # def readline(self):
    # def readlines(self):
    # def readinto1(self, b):
    # def close(self):
    # def readable(self):
    # def seekable(self):
    # def writable(self):
