// Reference source for handle_socket_data.abc. Not compiled at build time
// (aqw_patch embeds the .abc directly so building it doesn't need Java).
// Only the `handleSocketData` method body is ever extracted — the class
// name and its other members exist solely to make this compile standalone
// against Ruffle's own playerglobal signatures.
//
// Regenerate after editing this file:
//   java -cp ruffle/tools/asc/asc.jar macromedia.asc.embedding.ScriptCompiler \
//     -optimize -import <path-to-a-built>/playerglobal_import.abc \
//     -AS3 -strict handle_socket_data_source.as -out handle_socket_data
// (playerglobal_import.abc is produced by any `ruffle_core` build, at
// target/<profile>/build/ruffle_core-*/out/playerglobal_import.abc)
package {
    import flash.utils.ByteArray;
    import flash.net.Socket;
    import flash.events.Event;

    public class HSD {
        public var byteBuffer:ByteArray;
        public var socketConnection:Socket;

        public function handleMessage(param1:String) : void {
        }

        public function debugMessage(param1:String) : void {
        }

        public function handleSocketData(param1:Event) : void {
            var chunk:ByteArray = new ByteArray();
            var avail:int = int(this.socketConnection.bytesAvailable);
            this.socketConnection.readBytes(chunk, 0, avail);
            var start:int = 0;
            var i:int = 0;
            var len:int = chunk.length;
            var msgStr:String = null;
            while (i < len) {
                if (chunk[i] == 0) {
                    this.byteBuffer.writeBytes(chunk, start, i - start);
                    msgStr = this.byteBuffer.toString();
                    this.byteBuffer.clear();
                    start = i + 1;
                    try {
                        this.handleMessage(msgStr);
                    } catch (err:Error) {
                        this.debugMessage("handleMessage error: " + err.message);
                    }
                }
                i++;
            }
            if (start < len) {
                this.byteBuffer.writeBytes(chunk, start, len - start);
            }
        }
    }
}
