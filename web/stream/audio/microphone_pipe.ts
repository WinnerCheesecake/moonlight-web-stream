import { TrackAudioPlayer } from ".";
import { showNotification } from "../../component/notification";
import { globalObject } from "../../util";
import { Logger } from "../log";
import { Pipe, PipeInfo } from "../pipeline";
import { addPipePassthrough } from "../pipeline/pipes";

export class MicrophonePipe implements Pipe {
    static readonly pipeName = "MicrophonePipe"

    static readonly baseType = "audiotrack"
    static readonly type = "none"

    static async getInfo(): Promise<PipeInfo> {
        return {
            // This pipe only ever calls getUserMedia (see tryRequestMicrophone below) — an
            // earlier getDisplayMedia (screen share) check here was unrelated and could
            // report this pipe unsupported even when mic capture itself works fine.
            environmentSupported: "navigator" in globalObject() &&
                "mediaDevices" in navigator &&
                "getUserMedia" in navigator.mediaDevices
        }
    }

    readonly implementationName: string

    private logger: Logger | null = null

    private base: TrackAudioPlayer

    private microphoneTrack: MediaStreamTrack | null = null

    constructor(base: TrackAudioPlayer, logger?: Logger) {
        this.implementationName = `microphone -> ${base.implementationName}`

        this.logger = logger ?? null

        this.base = base

        addPipePassthrough(this)
    }

    private triedToAquire = false

    // Public (not private) so a UI button can trigger this directly, instead of relying
    // solely on it firing as a side effect of onUserInteraction() from an unrelated
    // click/keypress — that coupling is easy to miss, especially once a Guest has no
    // mouse/keyboard control by default (see control_gate on the server).
    async tryRequestMicrophone() {
        if (this.microphoneTrack || this.triedToAquire) {
            return
        }
        this.triedToAquire = true

        this.logger?.debug("Trying to aquire microphone")

        let microphone
        try {
            microphone = await navigator.mediaDevices.getUserMedia({ audio: true })
        } catch (e) {
            this.logger?.debug(`Failed to aquire microphone: ${e}`)
            showNotification(`Failed to aquire microphone: ${e}`)
            // Let the button be pressed again (e.g. after the user fixes a site permission).
            this.triedToAquire = false
            return
        }

        this.microphoneTrack = microphone.getAudioTracks()[0]
        this.base.setTrack(this.microphoneTrack)
        showNotification("Microphone connected", "info")
    }

    onUserInteraction() {
        this.tryRequestMicrophone()

        if ("onUserInteraction" in this.base && typeof this.base.onUserInteraction == "function") {
            return this.base.onUserInteraction(...arguments)
        }
    }

    getBase(): Pipe | null {
        return this.base
    }
}
