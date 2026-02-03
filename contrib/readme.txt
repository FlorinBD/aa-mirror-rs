sdr>audio focus>wait 600>open one ch   works
sdr>audio focus>wait 600>open all ch   works
sdr>audio focus>wait 600>open all ch>setup all works
sdr>audio focus>wait 600>open & setup all at once works

problems:
hu listener works only at first connection, after that it will close any attempt, why?
for scrcpy  we need a single thread, spawn only TcpStream's for reading

versions:
scrcpy-server version: 3.3.4

todo:
done: implement tokio_uring for adb reading task
done: replace postcard with BytesMut to have BE and correct layout, for SCRCPy control channel at least
done: replace ARP with ip neigh cli for ADB discovery
remove clone for structs that dosen't need to be clonable because is expensive, copy is ok
improve scrcpy audio/video reader to not re-alocate buf every time, use a single buffer, like payload
implement night/day switch for sensor_channel with:  "adb shell cmd uimode night yes"
signal all tasks that must be finished in a clean way
packet transmit should contain also encrypt to get rid of the new field