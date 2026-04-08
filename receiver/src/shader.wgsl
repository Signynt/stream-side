struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) in_vertex_index: u32) -> VertexOutput {
    var out: VertexOutput;

    // Fullscreen triangle
    let x = f32((in_vertex_index << 1u) & 2u);
    let y = f32(in_vertex_index & 2u);

    out.clip_position = vec4<f32>(
        x * 2.0 - 1.0,
        -(y * 2.0 - 1.0),
        0.0,
        1.0
    );

    out.tex_coords = vec2<f32>(x, y);
    return out;
}

// NV12 planes
@group(0) @binding(0) var t_y: texture_2d<f32>;
@group(0) @binding(1) var s_y: sampler;

// Вторая текстура содержит сразу U и V
@group(0) @binding(2) var t_uv: texture_2d<f32>;
@group(0) @binding(3) var s_uv: sampler;

// Убираем старые биндинги 4 и 5 (t_v, s_v), они больше не нужны

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // 1. Читаем яркость (Y)
    let y = textureSample(t_y, s_y, in.tex_coords).r;
    
    // 2. Читаем хроматическую составляющую (UV)
    // В формате NV12: Red канал текстуры — это U, Green — это V
    let uv = textureSample(t_uv, s_uv, in.tex_coords).rg;
    
    // Центрируем цветовые компоненты (они в диапазоне [0, 1], переводим в [-0.5, 0.5])
    let u = uv.r - 0.5;
    let v = uv.g - 0.5;

    // 3. Конвертация YUV -> RGB (BT.709 коэффициенты)
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;

    let rgb = vec3<f32>(r, g, b);

    // 4. Гамма-коррекция и финальный вывод
    // max(..., 0.0) нужен, чтобы pow не выдал ошибку на отрицательных значениях (бывают после YUV магии)
    let corrected_rgb = pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(2.2));
    
    return vec4<f32>(corrected_rgb, 1.0);
}