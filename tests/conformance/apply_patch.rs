pub const fn representative_patch() -> &'static str {
    concat!(
        "*** Begin Patch\n",
        "*** Add File: nested/hello.txt\n",
        "+Привет, мир!\n",
        "*** Update File: source.txt\n",
        "*** Move to: moved.txt\n",
        "@@\n",
        " alpha\n",
        "-beta\n",
        "+BETA\n",
        "@@\n",
        " gamma\n",
        "-delta\n",
        "+DELTA\n",
        "*** Delete File: delete.txt\n",
        "*** End Patch",
    )
}
