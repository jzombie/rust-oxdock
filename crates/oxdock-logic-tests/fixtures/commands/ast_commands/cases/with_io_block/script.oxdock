LET $block_outer: PIPE
LET $block_inner: PIPE
WITH_IO [stdout=$block_outer] {
    ECHO "outer-1"
    WITH_IO [stdout] ECHO "override-stdout"
    WITH_IO [stdout=$block_inner] {
        ECHO "inner-2"
    }
    ECHO "outer-3"
}

ECHO "outside"
