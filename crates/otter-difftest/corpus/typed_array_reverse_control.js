"use strict";
function checkTypedReverse(condition,message){if(!condition)throw new Error(message);}
for(const C of [Int8Array,Uint8Array,Uint8ClampedArray,Int16Array,Uint16Array,Int32Array,Uint32Array,Float16Array,Float32Array,Float64Array,BigInt64Array,BigUint64Array]){
    for(const count of [8,9]){
        const width=C.BYTES_PER_ELEMENT,buffer=new ArrayBuffer((count+2)*width);
        const view=new C(buffer,width,count),raw=new Uint8Array(buffer);
        for(let i=0;i<raw.length;++i)raw[i]=(i*37+11)%251;
        const before=Array.from(raw),method=view.reverse;
        Object.defineProperty(view,"length",{get(){throw new Error("native reverse cannot Get length");}});
        checkTypedReverse(method.call(view,{valueOf(){throw new Error("reverse cannot coerce arguments");}})===view,"receiver identity");
        for(let i=0;i<raw.length;++i){const element=Math.floor(i/width)-1,byte=i%width;const source=element>=0&&element<count?(count-element)*width+byte:i;checkTypedReverse(raw[i]===before[source],C.name+": exact element bytes");}
        method.call(view);for(let i=0;i<raw.length;++i)checkTypedReverse(raw[i]===before[i],"reverse twice restores bytes");
    }
}
for(const C of [Float16Array,Float32Array,Float64Array]){
    const width=C.BYTES_PER_ELEMENT,buffer=new ArrayBuffer(width*3),raw=new Uint8Array(buffer),data=new DataView(buffer);
    if(width===2){data.setUint16(0,0x7e19,true);data.setUint16(2,0x7c01,true);data.setUint16(4,0x8000,true);}
    if(width===4){data.setUint32(0,0x7fc01234,true);data.setUint32(4,0x7f800001,true);data.setUint32(8,0x80000000,true);}
    if(width===8){data.setUint32(0,0x12345678,true);data.setUint32(4,0x7ff80001,true);data.setUint32(8,1,true);data.setUint32(12,0x7ff00000,true);data.setUint32(20,0x80000000,true);}
    const before=Array.from(raw);new C(buffer).reverse();
    for(let i=0;i<raw.length;++i)checkTypedReverse(raw[i]===before[(2-Math.floor(i/width))*width+i%width],"NaN payload and signed zero");
}
{
    const buffer=new ArrayBuffer(8,{maxByteLength:16}),tracking=new Uint8Array(buffer);
    tracking.set([11,22,33,44,55,66,77,88]);buffer.resize(5);tracking.reverse();
    checkTypedReverse(tracking.join(",")==="55,44,33,22,11","tracking length");
    buffer.resize(8);const fixed=new Uint8Array(buffer,0,8);buffer.resize(5);
    let caught;try{fixed.reverse();}catch(error){caught=error;}checkTypedReverse(caught instanceof TypeError,"out-of-bounds fixed view");
    const detached=new Uint8Array(4);detached.buffer.transfer();caught=undefined;
    try{detached.reverse();}catch(error){caught=error;}checkTypedReverse(caught instanceof TypeError,"detached view");
    const shared=new Uint8Array(new SharedArrayBuffer(5));shared.set([11,22,33,44,55]);shared.reverse();
    checkTypedReverse(shared.join(",")==="55,44,33,22,11","shared odd view");
    for(const input of [new Uint8Array(0),new Uint8Array([19])])checkTypedReverse(input.reverse()===input,"short view identity");
}
console.log("ok:typed-reverse-control");
"ok:typed-reverse-control";
