void _start(unsigned int vga_offset) {
    volatile char *vga = (volatile char *)0xB8000;
    unsigned int offset = vga_offset;
    const char *msg = "Hello from C! (example program)";
    for (unsigned int i = 0; msg[i] != '\0'; i++) {
        vga[offset++] = msg[i];
        vga[offset++] = 0x0F;
    }
}
