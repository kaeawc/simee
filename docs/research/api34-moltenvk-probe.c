// Creates a graphics pipeline whose vertex input at location 2 is a float format while the
// shader reads it as uvec2 (what the API 34 guest's ANGLE asks for), and the same pipeline
// with a matching uint format as a control. Prints the VkResult of each. No device needed:
// it runs on the host's Vulkan drivers. See api34-moltenvk.md for the shaders and the run.
//
//   clang -I<Vulkan-Headers>/include -o probe api34-moltenvk-probe.c
//   VK_DRIVER_FILES=<lib64>/vulkan/MoltenVK_icd.json ./probe <lib64>/vulkan/libvulkan.dylib vs.spv fs.spv
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void *readall(const char *p, size_t *n) {
    FILE *f = fopen(p, "rb"); if (!f) { perror(p); exit(1); }
    fseek(f, 0, SEEK_END); *n = ftell(f); fseek(f, 0, SEEK_SET);
    void *b = malloc(*n); fread(b, 1, *n, f); fclose(f); return b;
}

int main(int argc, char **argv) {
    if (argc < 4) { fprintf(stderr, "usage: probe <libvulkan> <vs.spv> <fs.spv>\n"); return 2; }
    void *lib = dlopen(argv[1], RTLD_NOW); if (!lib) { fprintf(stderr, "%s\n", dlerror()); return 1; }
    PFN_vkGetInstanceProcAddr gipa = (PFN_vkGetInstanceProcAddr)dlsym(lib, "vkGetInstanceProcAddr");
#define I(name) PFN_##name name = (PFN_##name)gipa(inst, #name)
    VkInstance inst = VK_NULL_HANDLE;
    I(vkCreateInstance);
    const char *iext[] = {"VK_KHR_portability_enumeration"};
    VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO, 0, "probe", 1, "probe", 1, VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, 0, 0x00000001 /*ENUMERATE_PORTABILITY*/, &app, 0, 0, 1, iext};
    VkResult r = vkCreateInstance(&ici, 0, &inst); if (r) { printf("vkCreateInstance %d\n", r); return 1; }
    I(vkEnumeratePhysicalDevices); I(vkCreateDevice); I(vkGetPhysicalDeviceProperties);
    I(vkCreateShaderModule); I(vkCreateRenderPass); I(vkCreatePipelineLayout); I(vkCreateGraphicsPipelines);
    uint32_t n = 1; VkPhysicalDevice pd; vkEnumeratePhysicalDevices(inst, &n, &pd);
    VkPhysicalDeviceProperties props; vkGetPhysicalDeviceProperties(pd, &props);
    printf("device: %s (driver 0x%x)\n", props.deviceName, props.driverVersion);
    float prio = 1; VkDeviceQueueCreateInfo q = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, 0, 0, 0, 1, &prio};
    const char *dext[] = {"VK_KHR_portability_subset"};
    VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, 0, 0, 1, &q, 0, 0, 1, dext};
    VkDevice dev; r = vkCreateDevice(pd, &dci, 0, &dev);
    if (r == VK_ERROR_EXTENSION_NOT_PRESENT) { dci.enabledExtensionCount = 0; r = vkCreateDevice(pd, &dci, 0, &dev); }
    if (r) { printf("vkCreateDevice %d\n", r); return 1; }
    size_t vn, fn; void *vs = readall(argv[2], &vn), *fs = readall(argv[3], &fn);
    VkShaderModule vm, fm;
    VkShaderModuleCreateInfo smi = {VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, 0, 0, vn, vs};
    vkCreateShaderModule(dev, &smi, 0, &vm); smi.codeSize = fn; smi.pCode = fs; vkCreateShaderModule(dev, &smi, 0, &fm);
    VkAttachmentDescription att = {0, VK_FORMAT_R8G8B8A8_UNORM, VK_SAMPLE_COUNT_1_BIT, VK_ATTACHMENT_LOAD_OP_CLEAR,
        VK_ATTACHMENT_STORE_OP_STORE, VK_ATTACHMENT_LOAD_OP_DONT_CARE, VK_ATTACHMENT_STORE_OP_DONT_CARE,
        VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
    VkAttachmentReference ref = {0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
    VkSubpassDescription sp = {0, VK_PIPELINE_BIND_POINT_GRAPHICS, 0, 0, 1, &ref, 0, 0, 0, 0};
    VkRenderPassCreateInfo rpi = {VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO, 0, 0, 1, &att, 1, &sp, 0, 0};
    VkRenderPass rp; vkCreateRenderPass(dev, &rpi, 0, &rp);
    VkPipelineLayoutCreateInfo pli = {VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
    VkPipelineLayout pl; vkCreatePipelineLayout(dev, &pli, 0, &pl);
    VkPipelineShaderStageCreateInfo st[2] = {
        {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, 0, 0, VK_SHADER_STAGE_VERTEX_BIT, vm, "main", 0},
        {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, 0, 0, VK_SHADER_STAGE_FRAGMENT_BIT, fm, "main", 0}};
    struct { const char *name; VkFormat fmt; } cases[] = {
        {"location 2 uvec2 <- R32G32B32A32_SFLOAT (ANGLE's float current-value attribute)", VK_FORMAT_R32G32B32A32_SFLOAT},
        {"location 2 uvec2 <- R16G16_UINT (matching integer format, control)", VK_FORMAT_R16G16_UINT},
    };
    for (int i = 0; i < 2; i++) {
        VkVertexInputBindingDescription b[2] = {{0, 24, VK_VERTEX_INPUT_RATE_VERTEX}, {1, 16, VK_VERTEX_INPUT_RATE_VERTEX}};
        VkVertexInputAttributeDescription a[3] = {
            {0, 0, VK_FORMAT_R32G32_SFLOAT, 0}, {1, 0, VK_FORMAT_R8G8B8A8_UNORM, 8}, {2, 1, cases[i].fmt, 0}};
        VkPipelineVertexInputStateCreateInfo vi = {VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO, 0, 0, 2, b, 3, a};
        VkPipelineInputAssemblyStateCreateInfo ia = {VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO, 0, 0, VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST, 0};
        VkViewport vp = {0, 0, 64, 64, 0, 1}; VkRect2D sc = {{0, 0}, {64, 64}};
        VkPipelineViewportStateCreateInfo vps = {VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO, 0, 0, 1, &vp, 1, &sc};
        VkPipelineRasterizationStateCreateInfo rs = {VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO};
        rs.lineWidth = 1;
        VkPipelineMultisampleStateCreateInfo ms = {VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO};
        ms.rasterizationSamples = VK_SAMPLE_COUNT_1_BIT;
        VkPipelineColorBlendAttachmentState cba = {0}; cba.colorWriteMask = 0xf;
        VkPipelineColorBlendStateCreateInfo cb = {VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO};
        cb.attachmentCount = 1; cb.pAttachments = &cba;
        VkGraphicsPipelineCreateInfo gpi = {VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, 0, 0, 2, st, &vi, &ia, 0, &vps, &rs, &ms, 0, &cb, 0, pl, rp, 0, 0, -1};
        VkPipeline p = VK_NULL_HANDLE;
        r = vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &gpi, 0, &p);
        printf("%s: VkResult %d%s\n", cases[i].name, r, r ? " (FAILED)" : " (ok)");
    }
    return 0;
}
