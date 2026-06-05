MEMORY
{
    BOOTLOADER                        : ORIGIN = 0x00000000, LENGTH = 24K
    BOOTLOADER_STATE                  : ORIGIN = 0x00006000, LENGTH = 4K
    FLASH                             : ORIGIN = 0x00007000, LENGTH = 480K
    DFU                               : ORIGIN = 0x0007F000, LENGTH = 484K
    STORAGE                           : ORIGIN = 0x000F8000, LENGTH = 32K
    RAM                         (rwx) : ORIGIN = 0x20000000, LENGTH = 256K
}

__bootloader_state_start = ORIGIN(BOOTLOADER_STATE);
__bootloader_state_end = ORIGIN(BOOTLOADER_STATE) + LENGTH(BOOTLOADER_STATE);
__bootloader_dfu_start = ORIGIN(DFU);
__bootloader_dfu_end = ORIGIN(DFU) + LENGTH(DFU);
