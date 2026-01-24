sdr>audio focus>wait 600>open one ch   works
sdr>audio focus>wait 600>open all ch   works
sdr>audio focus>wait 600>open all ch>setup all works
sdr>audio focus>wait 600>open & setup all at once works

problems:
adb_client is 1.18 ATM because 1.19 requires rustc 1.91 and is not working with buildroot due to a libm.so.6 error
for scrcpy  we need a single thread, spawn only TcpStream's for reading

versions:
scrcpy-server version: 3.3.4

todo:
done: implement tokio_uring for adb reading task
replace postcard with BytesMut to have BE and correct layout, for SCRCPy control channel at least
replace ARP with ip neigh cli for ADB discovering